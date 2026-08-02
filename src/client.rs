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
        let mut reached_url = None;
        for attempt in 0..=self.retries {
            match self.client.get(url.clone()).send().await {
                Ok(response) if response.status().is_success() => {
                    let mut response = response;
                    let final_url = response.url().clone();
                    reached_url = Some(final_url.clone());
                    let mut bytes = Vec::new();
                    let mut body_error = None;
                    loop {
                        match response.chunk().await {
                            Ok(Some(chunk)) => {
                                if bytes.len() + chunk.len() > max_bytes {
                                    return Err(LjError::ResponseTooLarge {
                                        url: final_url.to_string(),
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
                    reached_url = Some(response.url().clone());
                    sleep(Duration::from_millis(150 * (attempt as u64 + 1))).await;
                }
                Ok(response) => {
                    return Err(LjError::HttpStatus {
                        url: response.url().to_string(),
                        status: response.status().as_u16(),
                    });
                }
                Err(source) => {
                    if let Some(later_url) = source
                        .url()
                        .filter(|source_url| *source_url != url)
                        .cloned()
                    {
                        reached_url = Some(later_url);
                    }
                    last_error = Some(source);
                    if attempt < self.retries {
                        sleep(Duration::from_millis(150 * (attempt as u64 + 1))).await;
                    }
                }
            }
        }
        Err(LjError::Http {
            url: reached_url.unwrap_or_else(|| url.clone()).to_string(),
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
                        let bytes_read = stream.read(&mut request).unwrap();
                        assert!(request[..bytes_read].starts_with(b"GET "));
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

    fn persistently_interrupted_redirect_server() -> (Url, Url, thread::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requested_url = Url::parse(&format!("{origin}/start")).unwrap();
        let final_url = Url::parse(&format!(
            "{origin}/delivered/interrupted.html?source=redirect"
        ))
        .unwrap();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut final_requests = 0;

            while final_requests < 3 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut request = [0; 1024];
                        let bytes_read = stream.read(&mut request).unwrap();
                        let request = &request[..bytes_read];
                        let response = if request.starts_with(b"GET /start ") {
                            b"HTTP/1.1 302 Found\r\nLocation: /delivered/interrupted.html?source=redirect\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                .as_slice()
                        } else {
                            assert!(
                                request.starts_with(
                                    b"GET /delivered/interrupted.html?source=redirect "
                                ),
                                "unexpected request: {}",
                                String::from_utf8_lossy(request)
                            );
                            final_requests += 1;
                            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nab"
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

            final_requests
        });
        (requested_url, final_url, server)
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
    async fn status_error_reports_final_redirect_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "/delivered/missing.html?source=status"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/delivered/missing.html"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = LjClient::new(None).unwrap();
        let requested_url = Url::parse(&format!("{}/start", server.uri())).unwrap();
        let final_url = format!("{}/delivered/missing.html?source=status", server.uri());

        let error = client.fetch_page(&requested_url).await.unwrap_err();

        match error {
            LjError::HttpStatus { url, status } => {
                assert_eq!(url, final_url);
                assert_eq!(status, 404);
            }
            other => panic!("expected status error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn response_too_large_reports_final_redirect_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/start"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "/delivered/big.html?source=size"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/delivered/big.html"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(100)))
            .mount(&server)
            .await;
        let client = LjClient::new(None).unwrap();
        let requested_url = Url::parse(&format!("{}/start", server.uri())).unwrap();
        let final_url = format!("{}/delivered/big.html?source=size", server.uri());

        let error = client
            .fetch_page_capped(&requested_url, 10)
            .await
            .unwrap_err();

        match error {
            LjError::ResponseTooLarge { url, limit } => {
                assert_eq!(url, final_url);
                assert_eq!(limit, 10);
            }
            other => panic!("expected response-too-large error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exhausted_interrupted_body_reports_final_redirect_url() {
        let (requested_url, final_url, server) = persistently_interrupted_redirect_server();
        let client = LjClient::new(None).unwrap();

        let error = client.fetch_page(&requested_url).await.unwrap_err();

        match error {
            LjError::Http { url, .. } => assert_eq!(url, final_url.as_str()),
            other => panic!("expected HTTP body error, got {other:?}"),
        }
        assert_eq!(server.join().unwrap(), 3);
    }

    #[tokio::test]
    async fn transport_error_without_response_reports_requested_url() {
        let unavailable = TcpListener::bind("127.0.0.1:0").unwrap();
        let requested_url = Url::parse(&format!(
            "http://{}/start",
            unavailable.local_addr().unwrap()
        ))
        .unwrap();
        drop(unavailable);
        let client = LjClient::new(None).unwrap();

        let error = client.fetch_page(&requested_url).await.unwrap_err();

        match error {
            LjError::Http { url, .. } => assert_eq!(url, requested_url.as_str()),
            other => panic!("expected transport error, got {other:?}"),
        }
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
