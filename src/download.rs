use super::base::credential::Credential;
use multipart::server::Multipart;
use once_cell::sync::Lazy;
use positioned_io::ReadAt;
use rand::{seq::SliceRandom, thread_rng};
use reqwest::blocking::Client as HTTPClient;
use std::{
    fs::OpenOptions,
    io::{
        copy as io_copy, Cursor, Error as IOError, ErrorKind as IOErrorKind, Result as IOResult,
        Write,
    },
    path::Path,
    result::Result,
    time::{Duration, SystemTime, SystemTimeError, UNIX_EPOCH},
};
use url::Url;

pub fn sign_download_url_with_deadline(
    c: &Credential,
    url: Url,
    deadline: SystemTime,
) -> Result<String, SystemTimeError> {
    let mut signed_url = url.to_string();

    if signed_url.contains('?') {
        signed_url.push_str("&e=");
    } else {
        signed_url.push_str("?e=");
    }

    let deadline = deadline.duration_since(UNIX_EPOCH)?.as_secs().to_string();
    signed_url.push_str(&deadline);
    let signature = c.sign(signed_url.as_bytes());
    signed_url.push_str("&token=");
    signed_url.push_str(&signature);
    Ok(signed_url)
}

pub fn sign_download_url_with_lifetime(
    c: &Credential,
    url: Url,
    lifetime: Duration,
) -> Result<String, SystemTimeError> {
    let deadline = SystemTime::now() + lifetime;
    sign_download_url_with_deadline(c, url, deadline)
}

static HTTP_CLIENT: Lazy<HTTPClient> = Lazy::new(Default::default);

#[derive(Debug)]
pub struct RangeReader {
    urls: Vec<String>,
    tries: usize,
}

impl RangeReader {
    pub fn new(urls: &[String], tries: usize) -> Self {
        assert!(tries > 0);
        Self {
            urls: urls.to_owned(),
            tries,
        }
    }

    pub fn new_from_key(
        key: impl AsRef<str>,
        domains: &[String],
        credential: &Credential,
        lifetime: Duration,
        tries: usize,
    ) -> Result<Self, SystemTimeError> {
        let urls = domains
            .iter()
            .map(|domain| {
                let url = domain.to_owned() + key.as_ref();
                let signed_url = sign_download_url_with_lifetime(
                    credential,
                    Url::parse(&url).unwrap(),
                    lifetime,
                )?;
                Ok(signed_url)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::new(&urls, tries))
    }
}

impl ReadAt for RangeReader {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> IOResult<usize> {
        let size = buf.len() as u64;
        let range = format!("bytes={}-{}", pos, pos + size - 1);
        let mut io_error: Option<IOError> = None;
        for url in self.choose_urls() {
            let result = HTTP_CLIENT
                .get(url)
                .header("Range", &range)
                .send()
                .and_then(|resp| {
                    let code = resp.status();
                    if code != 206 && code != 200 {
                        return Ok(0);
                    }
                    let bytes = resp.bytes()?;
                    buf.copy_from_slice(&bytes);
                    Ok(bytes.len())
                })
                .map_err(|err| IOError::new(IOErrorKind::Other, err));
            match result {
                Ok(size) => {
                    return Ok(size);
                }
                Err(err) => {
                    io_error = Some(err);
                }
            }
        }
        Err(io_error.unwrap())
    }
}

impl RangeReader {
    pub fn read_multi_range(&self, buf: &mut [u8], range: &[(u64, u64)]) -> IOResult<usize> {
        let range_header_value = format!("bytes={}", generate_range_header(range));
        let mut cursor = Cursor::new(buf);
        let mut io_error: Option<IOError> = None;

        for url in self.choose_urls() {
            cursor.set_position(0);

            let result = HTTP_CLIENT
                .get(url)
                .header("Range", &range_header_value)
                .send()
                .map_err(|err| IOError::new(IOErrorKind::Other, err))
                .and_then(|mut resp| {
                    let mut copy_error: Option<IOError> = None;
                    let code = resp.status();

                    if code == 200 {
                        let body = resp
                            .bytes()
                            .map_err(|err| IOError::new(IOErrorKind::BrokenPipe, err))?;
                        for &(from, len) in range.iter() {
                            let from = from as usize;
                            let len = len as usize;
                            cursor.write_all(&body.slice(from..(from + len)))?
                        }
                    } else {
                        if code != 206 {
                            return Ok(0);
                        }
                        let boundary = {
                            let content_type = resp
                                .headers()
                                .get("Content-Type")
                                .expect("Content-Type must be existed");
                            extract_boundary(
                                &content_type
                                    .to_str()
                                    .map_err(|err| IOError::new(IOErrorKind::Other, err))?,
                            )
                            .expect("Boundary must be existed in Content-Type")
                            .to_owned()
                        };

                        Multipart::with_body(&mut resp, boundary).foreach_entry(|mut field| {
                            if let Err(err) = io_copy(&mut field.data, &mut cursor) {
                                copy_error = Some(err);
                            }
                        })?;
                    }
                    copy_error.map_or(Ok(cursor.position() as usize), Err)
                });
            match result {
                Ok(size) => {
                    return Ok(size);
                }
                Err(err) => {
                    io_error = Some(err);
                }
            }
        }
        return Err(io_error.unwrap());

        fn generate_range_header(range: &[(u64, u64)]) -> String {
            range
                .iter()
                .map(|range| {
                    let start = range.0;
                    let end = start + range.1 - 1;
                    format!("{}-{}", start, end)
                })
                .collect::<Vec<_>>()
                .join(",")
        }

        fn extract_boundary<'s>(content_type: &'s str) -> Option<&'s str> {
            const BOUNDARY: &str = "boundary=";
            content_type.find(BOUNDARY).map(|idx| {
                let start = idx + BOUNDARY.len();
                let end = content_type[start..]
                    .find(';')
                    .map_or(content_type.len(), |end| start + end);
                &content_type[start..end]
            })
        }
    }

    pub fn exist(&self) -> IOResult<bool> {
        let mut io_error: Option<IOError> = None;
        for url in self.choose_urls() {
            let result = HTTP_CLIENT
                .head(url)
                .send()
                .map(|resp| resp.status() == 200)
                .map_err(|err| IOError::new(IOErrorKind::Other, err));
            match result {
                Ok(ok) => {
                    return Ok(ok);
                }
                Err(err) => {
                    io_error = Some(err);
                }
            }
        }
        return Err(io_error.unwrap());
    }

    pub fn download_to(&self, path: &Path) -> IOResult<u64> {
        let mut io_error: Option<IOError> = None;
        for url in self.choose_urls() {
            let result = HTTP_CLIENT
                .get(url)
                .send()
                .map_err(|err| IOError::new(IOErrorKind::Other, err))
                .and_then(|mut resp| {
                    if resp.status() != 200 {
                        return Err(IOError::new(IOErrorKind::Other, "Status Code is not 200"));
                    }
                    let content_length: Option<u64> = resp
                        .headers()
                        .get("Content-Length")
                        .and_then(|length| length.to_str().ok())
                        .and_then(|length| length.parse().ok());
                    let mut file = OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .truncate(true)
                        .open(path)?;
                    let copied = resp
                        .copy_to(&mut file)
                        .map_err(|err| IOError::new(IOErrorKind::BrokenPipe, err))?;
                    if let Some(content_length) = content_length {
                        if copied != content_length {
                            return Err(IOError::from(IOErrorKind::UnexpectedEof));
                        }
                    }
                    Ok(copied)
                });
            match result {
                Ok(downloaded) => {
                    return Ok(downloaded);
                }
                Err(err) => {
                    io_error = Some(err);
                }
            }
        }
        return Err(io_error.unwrap());
    }

    fn choose_urls(&self) -> Vec<&str> {
        let mut urls: Vec<&str> = self
            .urls
            .choose_multiple(&mut thread_rng(), self.tries)
            .map(|s| s.as_str())
            .collect();
        if urls.len() < self.tries {
            let still_needed: usize = self.tries - urls.len();
            for i in 0..still_needed {
                let index = i % self.urls.len();
                urls.push(urls[index]);
            }
        }
        urls
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::oneshot::channel;
    use multipart::client::lazy::Multipart;
    use reqwest::header::CONTENT_TYPE;
    use std::{
        boxed::Box,
        error::Error,
        io::Read,
        result::Result,
        sync::{Arc, Mutex},
    };
    use tokio::task::{spawn, spawn_blocking};
    use warp::{
        header,
        http::{HeaderValue, StatusCode},
        path,
        reply::Response,
        Filter,
    };

    macro_rules! starts_with_server {
        ($addr:ident, $routes:ident, $code:block) => {{
            let (tx, rx) = channel();
            let ($addr, server) =
                warp::serve($routes).bind_with_graceful_shutdown(([127, 0, 0, 1], 0), async move {
                    rx.await.ok();
                });
            let handler = spawn(server);
            $code;
            tx.send(()).ok();
            handler.await.ok();
        }};
    }

    #[tokio::test]
    async fn test_download_range() -> Result<(), Box<dyn Error>> {
        let routes = {
            path!("file")
                .and(header::value("Range"))
                .map(move |range: HeaderValue| {
                    assert_eq!(range.to_str().unwrap(), "bytes=0-4,5-9");
                    let mut response_body = Multipart::new();
                    response_body.add_text("0-4", "12345");
                    response_body.add_text("5-9", "67890");
                    let mut fields = response_body.prepare().unwrap();
                    let mut buffer = Vec::new();
                    fields.read_to_end(&mut buffer).unwrap();
                    let mut response = Response::new(buffer.into());
                    *response.status_mut() = StatusCode::PARTIAL_CONTENT;
                    response.headers_mut().insert(
                        CONTENT_TYPE,
                        ("multipart/form-data; boundary=".to_owned() + fields.boundary())
                            .parse()
                            .unwrap(),
                    );
                    response
                })
        };
        starts_with_server!(addr, routes, {
            let url = format!("http://{}/file", addr);
            let range_reader = RangeReader::new(&[url.to_owned()], 3);
            let buf = Arc::new(Mutex::new([0; 10]));
            let ranges = [(0, 5), (5, 5)];
            assert_eq!(
                {
                    let buf = buf.to_owned();
                    spawn_blocking(move || {
                        range_reader.read_multi_range(&mut *buf.lock().unwrap(), &ranges)
                    })
                    .await??
                },
                10
            );
            assert_eq!(&*buf.lock().unwrap(), b"1234567890");
        });
        Ok(())
    }

    #[test]
    fn test_sign_download_url_with_deadline() -> Result<(), Box<dyn Error>> {
        let credential = Credential::new("abcdefghklmnopq", "1234567890");
        assert_eq!(
            sign_download_url_with_deadline(&credential,
                Url::parse("http://www.qiniu.com/?go=1")?,
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_234_567_890 + 3600),
            )?,
            "http://www.qiniu.com/?go=1&e=1234571490&token=abcdefghklmnopq:KjQtlGAkEOhSwtFjJfYtYa2-reE=",
        );
        Ok(())
    }
}