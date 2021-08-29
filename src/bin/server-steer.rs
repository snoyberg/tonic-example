use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::Poll;

use hyper::server::conn::AddrStream;
use hyper::service::make_service_fn;
use hyper::HeaderMap;
use hyper::{body::HttpBody, Body, Request, Response};
use pin_project::pin_project;
use tonic::async_trait;
use tonic_example::echo_server::{Echo, EchoServer};
use tonic_example::{EchoReply, EchoRequest};
use tower::util::BoxService;
use tower::{steer::Steer, Service, ServiceExt};

struct MyEcho;

#[async_trait]
impl Echo for MyEcho {
    async fn echo(
        &self,
        request: tonic::Request<EchoRequest>,
    ) -> Result<tonic::Response<EchoReply>, tonic::Status> {
        Ok(tonic::Response::new(EchoReply {
            message: format!("Echoing back: {}", request.get_ref().message),
        }))
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;
type BoxResponse = Response<http_body::combinators::BoxBody<bytes::Bytes, BoxError>>;

#[tokio::main]
async fn main() {
    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));

    let axum_make_service = axum::Router::new()
        .route("/", axum::handler::get(|| async { "Hello world!" }))
        .into_make_service();

    let grpc_service = tonic::transport::Server::builder()
        .add_service(EchoServer::new(MyEcho))
        .into_service();

    let hybrid_make_service = make_service_fn(move |conn: &AddrStream| {
        let grpc_service = grpc_service.clone();
        async move {
            //let axum_make_service = axum_make_service.ready().await?;
            axum_make_service.call(()).await.map(|axum_service| {
                Steer::new(
                    vec![
                        BoxService::new(
                            axum_service
                                .map_response(|r| r.map(HybridBody::Web))
                                .map_err(BoxError::from),
                        ),
                        BoxService::new(
                            grpc_service
                                .map_response(|r| r.map(HybridBody::Grpc))
                                .map_err(BoxError::from),
                        ),
                    ],
                    |req: &Request<Body>, _services| {
                        if req.headers().get("content-type").map(|x| x.as_bytes())
                            == Some(b"application/grpc")
                        {
                            1
                        } else {
                            0
                        }
                    },
                )
            })
        }
    });

    let server = hyper::Server::bind(&addr).serve(hybrid_make_service);

    if let Err(e) = server.await {
        eprintln!("server error: {}", e);
    }
}

fn hybrid<MakeWeb, Grpc>(make_web: MakeWeb, grpc: Grpc) -> HybridMakeService<MakeWeb, Grpc> {
    HybridMakeService { make_web, grpc }
}

struct HybridMakeService<MakeWeb, Grpc> {
    make_web: MakeWeb,
    grpc: Grpc,
}

impl<ConnInfo, MakeWeb, Grpc> Service<ConnInfo> for HybridMakeService<MakeWeb, Grpc>
where
    MakeWeb: Service<ConnInfo>,
    Grpc: Clone,
{
    type Response = HybridService<MakeWeb::Response, Grpc>;
    type Error = MakeWeb::Error;
    type Future = HybridMakeServiceFuture<MakeWeb::Future, Grpc>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.make_web.poll_ready(cx)
    }

    fn call(&mut self, conn_info: ConnInfo) -> Self::Future {
        HybridMakeServiceFuture {
            web_future: self.make_web.call(conn_info),
            grpc: Some(self.grpc.clone()),
        }
    }
}

#[pin_project]
struct HybridMakeServiceFuture<WebFuture, Grpc> {
    #[pin]
    web_future: WebFuture,
    grpc: Option<Grpc>,
}

impl<WebFuture, Web, WebError, Grpc> Future for HybridMakeServiceFuture<WebFuture, Grpc>
where
    WebFuture: Future<Output = Result<Web, WebError>>,
{
    type Output = Result<HybridService<Web, Grpc>, WebError>;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
        let this = self.project();
        match this.web_future.poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(web)) => Poll::Ready(Ok(HybridService {
                web,
                grpc: this.grpc.take().expect("Cannot poll twice!"),
            })),
        }
    }
}

struct HybridService<Web, Grpc> {
    web: Web,
    grpc: Grpc,
}

impl<Web, Grpc, WebBody, GrpcBody> Service<Request<Body>> for HybridService<Web, Grpc>
where
    Web: Service<Request<Body>, Response = Response<WebBody>>,
    Grpc: Service<Request<Body>, Response = Response<GrpcBody>>,
    Web::Error: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    Grpc::Error: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
{
    type Response = Response<HybridBody<WebBody, GrpcBody>>;
    type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
    type Future = HybridFuture<Web::Future, Grpc::Future>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match self.web.poll_ready(cx) {
            Poll::Ready(Ok(())) => match self.grpc.poll_ready(cx) {
                Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
                Poll::Pending => Poll::Pending,
            },
            Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
            Poll::Pending => Poll::Pending,
        }
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        if req.headers().get("content-type").map(|x| x.as_bytes()) == Some(b"application/grpc") {
            HybridFuture::Grpc(self.grpc.call(req))
        } else {
            HybridFuture::Web(self.web.call(req))
        }
    }
}

#[pin_project(project = HybridBodyProj)]
enum HybridBody<WebBody, GrpcBody> {
    Web(#[pin] WebBody),
    Grpc(#[pin] GrpcBody),
}

impl<WebBody, GrpcBody> HttpBody for HybridBody<WebBody, GrpcBody>
where
    WebBody: HttpBody + Send + Unpin,
    GrpcBody: HttpBody<Data = WebBody::Data> + Send + Unpin,
    WebBody::Error: std::error::Error + Send + Sync + 'static,
    GrpcBody::Error: std::error::Error + Send + Sync + 'static,
{
    type Data = WebBody::Data;
    type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

    fn is_end_stream(&self) -> bool {
        match self {
            HybridBody::Web(b) => b.is_end_stream(),
            HybridBody::Grpc(b) => b.is_end_stream(),
        }
    }

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        match self.project() {
            HybridBodyProj::Web(b) => b.poll_data(cx).map_err(|e| e.into()),
            HybridBodyProj::Grpc(b) => b.poll_data(cx).map_err(|e| e.into()),
        }
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        match self.project() {
            HybridBodyProj::Web(b) => b.poll_trailers(cx).map_err(|e| e.into()),
            HybridBodyProj::Grpc(b) => b.poll_trailers(cx).map_err(|e| e.into()),
        }
    }
}

#[pin_project(project = HybridFutureProj)]
enum HybridFuture<WebFuture, GrpcFuture> {
    Web(#[pin] WebFuture),
    Grpc(#[pin] GrpcFuture),
}

impl<WebFuture, GrpcFuture, WebBody, GrpcBody, WebError, GrpcError> Future
    for HybridFuture<WebFuture, GrpcFuture>
where
    WebFuture: Future<Output = Result<Response<WebBody>, WebError>>,
    GrpcFuture: Future<Output = Result<Response<GrpcBody>, GrpcError>>,
    WebError: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    GrpcError: Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
{
    type Output = Result<
        Response<HybridBody<WebBody, GrpcBody>>,
        Box<dyn std::error::Error + Send + Sync + 'static>,
    >;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context) -> Poll<Self::Output> {
        match self.project() {
            HybridFutureProj::Web(a) => match a.poll(cx) {
                Poll::Ready(Ok(res)) => Poll::Ready(Ok(res.map(HybridBody::Web))),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
                Poll::Pending => Poll::Pending,
            },
            HybridFutureProj::Grpc(b) => match b.poll(cx) {
                Poll::Ready(Ok(res)) => Poll::Ready(Ok(res.map(HybridBody::Grpc))),
                Poll::Ready(Err(e)) => Poll::Ready(Err(e.into())),
                Poll::Pending => Poll::Pending,
            },
        }
    }
}
