use std::time::Duration;

use tokio::time::sleep;
use url::Url;

use crate::error::{LjError, Result};

const MAX_PAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct FetchedPage {
    pub final_url: Url,
    pub text: String,
}

#[derive(Clone)]
pub struct LjClient {
    client: reqwest::Client,
    retries: usize,
}

impl LjClient {
    pub fn new(user_agent: Option<&str>) -> Result<Self> {
        let client = crate::net::configured_client_builder(user_agent)
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .map_err(|source| LjError::Http {
                url: "client builder".to_string(),
                source,
            })?;
        Ok(Self { client, retries: 2 })
    }

    pub async fn fetch_text(&self, url: &Url) -> Result<String> {
        Ok(self.fetch_page(url).await?.text)
    }

    pub async fn fetch_page(&self, url: &Url) -> Result<FetchedPage> {
        self.fetch_page_capped(url, MAX_PAGE_BYTES).await
    }

    async fn fetch_page_capped(&self, url: &Url, max_bytes: usize) -> Result<FetchedPage> {
        let mut last_error = None;
        for attempt in 0..=self.retries {
            match self.client.get(url.clone()).send().await {
                Ok(response) if response.status().is_success() => {
                    let mut response = response;
                    let final_url = response.url().clone();
                    let mut bytes = Vec::new();
                    let mut body_error = None;
                    loop {
                        match response.chunk().await {
                            Ok(Some(chunk)) => {
                                if bytes.len() + chunk.len() > max_bytes {
                                    return Err(LjError::ResponseTooLarge {
                                        url: url.to_string(),
                                        limit: max_bytes,
                                    });
                                }
                                bytes.extend_from_slice(&chunk);
                            }
                            Ok(None) => break,
                            Err(source) => {
                                body_error = Some(source);
                                break;
                            }
                        }
                    }
                    if let Some(source) = body_error {
                        last_error = Some(source);
                        if attempt < self.retries {
                            sleep(Duration::from_millis(150 * (attempt as u64 + 1))).await;
                        }
                        continue;
                    }
                    return Ok(FetchedPage {
                        final_url,
                        text: String::from_utf8_lossy(&bytes).into_owned(),
                    });
                }
                Ok(response) if response.status().is_server_error() && attempt < self.retries => {
                    sleep(Duration::from_millis(150 * (attempt as u64 + 1))).await;
                }
                Ok(response) => {
                    return Err(LjError::HttpStatus {
                        url: url.to_string(),
                        status: response.status().as_u16(),
                    });
                }
                Err(source) => {
                    last_error = Some(source);
                    if attempt < self.retries {
                        sleep(Duration::from_millis(150 * (attempt as u64 + 1))).await;
                    }
                }
            }
        }
        Err(LjError::Http {
            url: url.to_string(),
            source: last_error.expect("retry loop always records final error"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Instant,
    };
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    fn interrupted_body_server() -> (Url, thread::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = Url::parse(&format!(
            "http://{}/interrupted.html",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut requests = 0;

            while requests < 2 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut request = [0; 1024];
                        stream.read(&mut request).unwrap();
                        requests += 1;
                        let response = if requests == 1 {
                            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nab"
                                .as_slice()
                        } else {
                            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello"
                                .as_slice()
                        };
                        stream.write_all(response).unwrap();
                        stream.flush().unwrap();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            }

            requests
        });
        (url, server)
    }

    #[tokio::test]
    async fn fetch_page_reports_final_redirect_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", "/final"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/final"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let client = LjClient::new(None).unwrap();
        let start = Url::parse(&format!("{}/start", server.uri())).unwrap();
        let page = client.fetch_page(&start).await.unwrap();

        assert_eq!(page.final_url.path(), "/final");
        assert_eq!(page.text, "ok");
    }

    #[tokio::test]
    async fn retries_interrupted_body_from_empty_buffer() {
        let (url, server) = interrupted_body_server();
        let client = LjClient::new(None).unwrap();

        let text = client.fetch_text(&url).await.unwrap();

        assert_eq!(text, "hello");
        assert_eq!(server.join().unwrap(), 2);
    }

    #[tokio::test]
    async fn sends_no_user_agent_and_no_cookie_to_non_livejournal_host() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/post.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let client = LjClient::new(None).unwrap();
        let text = client
            .fetch_text(&Url::parse(&format!("{}/post.html", server.uri())).unwrap())
            .await
            .unwrap();
        assert_eq!(text, "ok");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        // No User-Agent on any request.
        assert!(requests[0].headers.get("user-agent").is_none());
        // The affirmation cookie is scoped to livejournal.com, so it is NOT
        // sent to the loopback test server (nor to any third-party host).
        assert!(requests[0].headers.get("cookie").is_none());
    }

    #[tokio::test]
    async fn sends_user_agent_when_set() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/post.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let client = LjClient::new(Some("lj-archiver-test/1.0")).unwrap();
        client
            .fetch_text(&Url::parse(&format!("{}/post.html", server.uri())).unwrap())
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests[0].headers.get("user-agent").unwrap(),
            "lj-archiver-test/1.0"
        );
    }

    #[tokio::test]
    async fn rejects_error_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/missing.html"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = LjClient::new(None).unwrap();
        let error = client
            .fetch_text(&Url::parse(&format!("{}/missing.html", server.uri())).unwrap())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("404"));
    }

    #[tokio::test]
    async fn retries_on_server_error_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/flaky.html"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/flaky.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let client = LjClient::new(None).unwrap();
        let text = client
            .fetch_text(&Url::parse(&format!("{}/flaky.html", server.uri())).unwrap())
            .await
            .unwrap();
        assert_eq!(text, "ok");
    }

    #[tokio::test]
    async fn exhausts_retries_on_persistent_server_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/down.html"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let client = LjClient::new(None).unwrap();
        let err = client
            .fetch_text(&Url::parse(&format!("{}/down.html", server.uri())).unwrap())
            .await
            .unwrap_err();
        // Persistent 5xx: retries are exhausted and the final response surfaces as
        // HttpStatus (not a panic on last_error.expect, not LjError::Http, no hang).
        match err {
            LjError::HttpStatus { status, .. } => assert_eq!(status, 503),
            other => panic!("expected HttpStatus 503, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_page_larger_than_cap() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/big.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(100)))
            .mount(&server)
            .await;
        let client = LjClient::new(None).unwrap();
        let err = client
            .fetch_page_capped(
                &Url::parse(&format!("{}/big.html", server.uri())).unwrap(),
                10,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeded"));
    }
}
