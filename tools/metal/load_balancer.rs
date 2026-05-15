use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body as _, Incoming};
use hyper::http;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::convert::Infallible;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type BoxBody = http_body_util::combinators::BoxBody<Bytes, BoxError>;
type HttpClient = Client<HttpConnector, Incoming>;

pub trait Resolver: Send + Sync + 'static {
    fn resolve(&self, host: &str) -> Option<(std::net::IpAddr, u16)>;
}

#[derive(Clone)]
pub struct TlsConfig {
    config: Arc<rustls::ServerConfig>,
}

pub fn load_tls_config(
    cert_chain_path: &Path,
    private_key_path: &Path,
) -> std::io::Result<TlsConfig> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cert_chain = load_cert_chain(cert_chain_path)?;
    let private_key = load_private_key(private_key_path)?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, private_key)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(TlsConfig {
        config: Arc::new(config),
    })
}

fn load_cert_chain(path: &Path) -> std::io::Result<Vec<CertificateDer<'static>>> {
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader).collect::<Result<_, _>>()?;
    if certs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} does not contain a PEM certificate", path.display()),
        ));
    }
    Ok(certs)
}

fn load_private_key(path: &Path) -> std::io::Result<PrivateKeyDer<'static>> {
    let mut reader = BufReader::new(std::fs::File::open(path)?);
    rustls_pemfile::private_key(&mut reader)?.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} does not contain a PEM private key", path.display()),
        )
    })
}

fn empty_response(status: http::StatusCode) -> Response<BoxBody> {
    Response::builder()
        .status(status)
        .body(
            Full::new(Bytes::new())
                .map_err(|never| match never {})
                .boxed(),
        )
        .unwrap()
}

fn bytes_response(status: http::StatusCode, body: Bytes) -> Response<BoxBody> {
    Response::builder()
        .status(status)
        .body(Full::new(body).map_err(|never| match never {}).boxed())
        .unwrap()
}

fn extract_host<T>(req: &hyper::Request<T>) -> Option<String> {
    if let Some(host_header) = req.headers().get(http::header::HOST) {
        if let Ok(auth) = host_header
            .to_str()
            .ok()?
            .parse::<http::uri::Authority>()
        {
            return Some(auth.host().to_string());
        }
    }
    req.uri().authority().map(|auth| auth.host().to_string())
}

async fn serve<F, Fut>(port: u16, handler: F)
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = Result<Response<BoxBody>, Infallible>> + Send + 'static,
{
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("server bind error: {e:?}");
            return;
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
        tokio::spawn(async move {
            let service = service_fn(handler);
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                eprintln!("server connection error: {e:?}");
            }
        });
    }
}

pub async fn handle_http(port: u16, root_dir: Option<std::path::PathBuf>) {
    serve(port, move |req| {
        let root_dir = root_dir.clone();
        async move {
            let mut parts = http::uri::Parts::from(req.uri().clone());

            if let Some(root_dir) = root_dir.as_ref() {
                if let Some(pq) = parts.path_and_query.as_ref() {
                    if pq.path().starts_with("/.well-known/") {
                        let path = match root_dir.join(&pq.path()[1..]).canonicalize() {
                            Ok(path) => path,
                            Err(_) => {
                                return Ok(empty_response(http::StatusCode::NOT_FOUND));
                            }
                        };

                        if !path.starts_with(root_dir) {
                            return Ok(empty_response(http::StatusCode::NOT_FOUND));
                        }

                        return Ok(match std::fs::read(path) {
                            Ok(content) => {
                                bytes_response(http::StatusCode::OK, Bytes::from(content))
                            }
                            Err(_) => empty_response(http::StatusCode::NOT_FOUND),
                        });
                    }
                }
            }

            parts.scheme = Some("https".parse().unwrap());
            if parts.authority.is_none() {
                let Some(host) = extract_host(&req) else {
                    return Ok(empty_response(http::StatusCode::BAD_REQUEST));
                };
                parts.authority = Some(host.parse().unwrap());
            }

            let uri = hyper::Uri::from_parts(parts).unwrap().to_string();
            let mut resp = empty_response(http::StatusCode::TEMPORARY_REDIRECT);
            resp.headers_mut().insert(
                hyper::header::LOCATION,
                hyper::header::HeaderValue::from_bytes(uri.as_bytes()).unwrap(),
            );
            Ok(resp)
        }
    })
    .await;
}

pub async fn proxy(port: u16, resolver: std::sync::Arc<dyn Resolver>) {
    let client = Client::builder(TokioExecutor::new()).build_http();
    serve(port, move |req| {
        let resolver = resolver.clone();
        let client: HttpClient = client.clone();
        async move { proxy_request(req, resolver, client).await }
    })
    .await;
}

pub async fn tls_proxy(port: u16, tls_config: TlsConfig, resolver: Arc<dyn Resolver>) {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("tls server bind error: {e:?}");
            return;
        }
    };
    let acceptor = TlsAcceptor::from(tls_config.config);
    let client = Client::builder(TokioExecutor::new()).build_http();

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(stream) => stream,
            Err(e) => {
                eprintln!("tls server accept error: {e:?}");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let resolver = resolver.clone();
        let client: HttpClient = client.clone();
        tokio::spawn(async move {
            let stream = match acceptor.accept(stream).await {
                Ok(stream) => stream,
                Err(e) => {
                    eprintln!("tls accept error: {e:?}");
                    return;
                }
            };
            let service =
                service_fn(move |req| proxy_request(req, resolver.clone(), client.clone()));
            if let Err(e) = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                eprintln!("tls server connection error: {e:?}");
            }
        });
    }
}

async fn proxy_request(
    mut req: Request<Incoming>,
    resolver: Arc<dyn Resolver>,
    client: HttpClient,
) -> Result<Response<BoxBody>, Infallible> {
    let Some(host) = extract_host(&req) else {
        return Ok(empty_response(http::StatusCode::BAD_REQUEST));
    };

    let Some((ip, port)) = resolver.resolve(&host) else {
        return Ok(empty_response(http::StatusCode::NOT_FOUND));
    };

    let mut parts = http::uri::Parts::from(req.uri().clone());
    parts.authority = Some(format!("{ip}:{port}").parse().unwrap());
    parts.scheme = Some("http".parse().unwrap());
    *req.uri_mut() = http::uri::Uri::from_parts(parts).unwrap();

    match client.request(req).await {
        Ok(resp) => Ok(resp.map(|body| body.map_err(|e| -> BoxError { Box::new(e) }).boxed())),
        Err(e) => {
            eprintln!("proxy request error: {e:?}");
            Ok(empty_response(http::StatusCode::BAD_GATEWAY))
        }
    }
}
