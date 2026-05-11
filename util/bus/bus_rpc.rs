use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Body as _, Frame, Incoming};
use hyper::service::service_fn;
use hyper::{http, Request, Response};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::connect::{Connect, HttpConnector};
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;

mod metal;
pub use metal::MetalAsyncClient;

type ResponseBody = http_body_util::combinators::BoxBody<Bytes, Infallible>;

pub async fn serve<H: bus::BusAsyncServer + 'static>(port: u16, handler: H) -> bus::BusRpcError {
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("server bind error: {e:?}");
            return bus::BusRpcError::FailedToBindPort;
        }
    };

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("server accept error: {e:?}");
                continue;
            }
        };
        let handler = handler.clone();
        tokio::task::spawn(async move {
            let service = service_fn(move |req| handle_request(req, handler.clone()));
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                eprintln!("server connection error: {e:?}");
            }
        });
    }
}

async fn handle_request<H: bus::BusAsyncServer + 'static>(
    req: Request<Incoming>,
    handler: H,
) -> Result<Response<ResponseBody>, Infallible> {
    let is_stream = req.headers().contains_key("bus-stream");

    let mut iter = req.uri().path().split("/");
    let (service, method) = match (iter.next(), iter.next(), iter.next(), iter.next()) {
        (Some(""), Some(service), Some(method), None) => {
            (service.to_string(), method.to_string())
        }
        _ => return Ok(response(http::StatusCode::NOT_FOUND, Bytes::new())),
    };

    let payload = match req.into_body().collect().await {
        Ok(payload) => payload.to_bytes().to_vec(),
        Err(_) => return Ok(response(http::StatusCode::BAD_REQUEST, Bytes::new())),
    };

    if is_stream {
        let (sink, rec) = bus::BusSinkBase::new();
        tokio::task::spawn(async move {
            handler
                .serve_stream(&service, &method, &payload, sink)
                .await
        });
        let stream = rec.map(|item| {
            let data = item.unwrap_or_else(|_| Vec::new());
            Ok::<_, Infallible>(Frame::data(Bytes::from(data)))
        });
        let body = BodyExt::boxed(StreamBody::new(stream));
        return Ok(Response::builder()
            .status(http::StatusCode::OK)
            .body(body)
            .unwrap());
    }

    match handler.serve(&service, &method, &payload).await {
        Ok(data) => Ok(response(http::StatusCode::OK, Bytes::from(data))),
        Err(bus::BusRpcError::NotImplemented) => {
            Ok(response(http::StatusCode::NOT_IMPLEMENTED, Bytes::new()))
        }
        Err(bus::BusRpcError::ServiceNameDidNotMatch) => {
            Ok(response(http::StatusCode::NOT_FOUND, Bytes::new()))
        }
        Err(_) => Ok(response(
            http::StatusCode::INTERNAL_SERVER_ERROR,
            Bytes::new(),
        )),
    }
}

fn response(status: http::StatusCode, body: Bytes) -> Response<ResponseBody> {
    Response::builder()
        .status(status)
        .body(Full::new(body).boxed())
        .unwrap()
}

pub struct HyperSyncClient<T>
where
    T: Connect + Clone + Send + Sync + 'static,
{
    inner: HyperClient<T>,
    executor: tokio::runtime::Runtime,
}

pub struct HyperClientInner<T>
where
    T: Connect + Clone + Send + Sync + 'static,
{
    host: String,
    port: u16,
    client: Client<T, Full<Bytes>>,
    use_tls: bool,
    headers: Vec<(hyper::header::HeaderName, String)>,
}

#[derive(Clone)]
pub struct HyperClient<T>
where
    T: Connect + Clone + Send + Sync + 'static,
{
    inner: Arc<HyperClientInner<T>>,
}

impl HyperSyncClient<HttpConnector> {
    pub fn new(host: String, port: u16) -> Self {
        let executor = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        HyperSyncClient {
            inner: HyperClient::new(host, port),
            executor,
        }
    }
}

impl HyperSyncClient<hyper_rustls::HttpsConnector<HttpConnector>> {
    pub fn new_tls(host: String, port: u16) -> Self {
        let executor = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        HyperSyncClient {
            inner: HyperClient::new_tls(host, port),
            executor,
        }
    }
}

impl HyperClient<HttpConnector> {
    pub fn new(host: String, port: u16) -> Self {
        let connector = HttpConnector::new();
        HyperClient {
            inner: Arc::new(HyperClientInner {
                host,
                port,
                client: Client::builder(TokioExecutor::new()).build(connector),
                use_tls: false,
                headers: Vec::new(),
            }),
        }
    }
}

impl HyperClient<hyper_rustls::HttpsConnector<HttpConnector>> {
    pub fn new_tls(host: String, port: u16) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let connector = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_only()
            .enable_http1()
            .build();
        HyperClient {
            inner: Arc::new(HyperClientInner {
                host,
                port,
                client: Client::builder(TokioExecutor::new()).build(connector),
                use_tls: true,
                headers: Vec::new(),
            }),
        }
    }
}

impl<T> HyperClient<T>
where
    T: Connect + Clone + Send + Sync + 'static,
{
    pub fn add_header(&mut self, header: hyper::header::HeaderName, value: String) {
        Arc::get_mut(&mut self.inner)
            .unwrap()
            .headers
            .push((header, value));
    }
}

impl<T> HyperSyncClient<T>
where
    T: Connect + Clone + Send + Sync + 'static,
{
    pub fn add_header(&mut self, header: hyper::header::HeaderName, value: String) {
        self.inner.add_header(header, value);
    }
}

struct BusStream {
    body: std::pin::Pin<Box<Incoming>>,
    size: Option<usize>,
    buffer: VecDeque<u8>,
}

impl BusStream {
    fn new(r: Response<Incoming>) -> Self {
        Self {
            body: Box::pin(r.into_body()),
            size: None,
            buffer: VecDeque::new(),
        }
    }
}

impl Stream for BusStream {
    type Item = Result<Vec<u8>, String>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut futures::task::Context<'_>,
    ) -> futures::task::Poll<Option<Self::Item>> {
        loop {
            if let Some(len) = self.size {
                if self.buffer.len() >= len {
                    let mut out = self.buffer.split_off(len);
                    std::mem::swap(&mut out, &mut self.buffer);
                    self.size = None;
                    return futures::task::Poll::Ready(Some(Ok(Vec::from(out))));
                }
            } else if self.buffer.len() >= 4 {
                self.size = Some(u32::from_le_bytes([
                    self.buffer.pop_front().unwrap(),
                    self.buffer.pop_front().unwrap(),
                    self.buffer.pop_front().unwrap(),
                    self.buffer.pop_front().unwrap(),
                ]) as usize);
                continue;
            }

            match self.body.as_mut().poll_frame(cx) {
                futures::task::Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data() {
                        self.buffer.extend(data.iter());
                    }
                }
                futures::task::Poll::Ready(Some(Err(e))) => {
                    return futures::task::Poll::Ready(Some(Err(format!("{e:?}"))))
                }
                futures::task::Poll::Ready(None) => {
                    if self.buffer.is_empty() {
                        return futures::task::Poll::Ready(None);
                    }
                    return futures::task::Poll::Ready(Some(Err(
                        "truncated bus stream response".to_string(),
                    )));
                }
                futures::task::Poll::Pending => return futures::task::Poll::Pending,
            }
        }
    }
}

impl<T> HyperClient<T>
where
    T: Connect + Clone + Send + Sync + 'static,
{
    async fn request_async(&self, uri: &str, data: Vec<u8>) -> Result<Vec<u8>, bus::BusRpcError> {
        let builder = hyper::Uri::builder();
        let builder = if self.inner.use_tls {
            builder.scheme("https")
        } else {
            builder.scheme("http")
        };
        let uri = builder
            .authority(format!("{}:{}", self.inner.host, self.inner.port))
            .path_and_query(uri)
            .build()
            .map_err(|e| bus::BusRpcError::InternalError(format!("{e:?}")))?;

        let mut req = hyper::Request::builder()
            .method("POST")
            .uri(uri)
            .body(Full::new(Bytes::from(data)))
            .map_err(|e| bus::BusRpcError::InternalError(format!("{e:?}")))?;

        for (header, value) in &self.inner.headers {
            req.headers_mut().insert(
                header,
                hyper::header::HeaderValue::from_bytes(value.as_bytes()).unwrap(),
            );
        }

        let resp = self
            .inner
            .client
            .request(req)
            .await
            .map_err(|e| bus::BusRpcError::ConnectionError(format!("{e:?}")))?;

        Ok(resp
            .into_body()
            .collect()
            .await
            .map_err(|e| bus::BusRpcError::ConnectionError(format!("{e:?}")))?
            .to_bytes()
            .to_vec())
    }

    async fn stream_async(&self, uri: &str, data: Vec<u8>) -> Result<BusStream, bus::BusRpcError> {
        let builder = hyper::Uri::builder();
        let builder = if self.inner.use_tls {
            builder.scheme("https")
        } else {
            builder.scheme("http")
        };
        let uri = builder
            .authority(format!("{}:{}", self.inner.host, self.inner.port))
            .path_and_query(uri)
            .build()
            .map_err(|e| bus::BusRpcError::InternalError(format!("{e:?}")))?;

        let mut req = hyper::Request::builder()
            .method("POST")
            .uri(uri)
            .body(Full::new(Bytes::from(data)))
            .map_err(|e| bus::BusRpcError::InternalError(format!("{e:?}")))?;

        req.headers_mut().insert(
            hyper::header::HeaderName::from_static("bus-stream"),
            hyper::header::HeaderValue::from_bytes("1".as_bytes()).unwrap(),
        );

        for (header, value) in &self.inner.headers {
            req.headers_mut().insert(
                header,
                hyper::header::HeaderValue::from_bytes(value.as_bytes()).unwrap(),
            );
        }

        let resp = self
            .inner
            .client
            .request(req)
            .await
            .map_err(|e| bus::BusRpcError::ConnectionError(format!("{e:?}")))?;

        Ok(BusStream::new(resp))
    }
}

impl<T> bus::BusClient for HyperSyncClient<T>
where
    T: Connect + Clone + Send + Sync + 'static,
{
    fn request(&self, uri: &str, data: Vec<u8>) -> Result<Vec<u8>, bus::BusRpcError> {
        self.executor
            .block_on(async { self.inner.request_async(uri, data).await })
    }
}

impl<T> bus::BusAsyncClient for HyperClient<T>
where
    T: Connect + Clone + Send + Sync + 'static,
{
    fn request(
        &self,
        uri: &'static str,
        data: Vec<u8>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>, bus::BusRpcError>> + Send>>
    {
        let self_ = self.clone();
        Box::pin(async move { self_.request_async(uri, data).await })
    }

    fn request_stream(
        &self,
        uri: &'static str,
        data: Vec<u8>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        std::pin::Pin<Box<dyn Stream<Item = Result<Vec<u8>, String>> + Send>>,
                        bus::BusRpcError,
                    >,
                > + Send,
        >,
    > {
        let self_ = self.clone();
        Box::pin(async move {
            self_.stream_async(uri, data).await.map(|r| {
                let o: std::pin::Pin<Box<dyn Stream<Item = Result<Vec<u8>, String>> + Send>> =
                    Box::pin(r);
                o
            })
        })
    }
}
