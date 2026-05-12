use axum::body::Body;
use http::{Method, Request, Response};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::{Layer, Service};

/// Tower layer that rewrites the HTTP `SOURCE` method to `PUT` so axum's
/// router can match it. Icecast sources use `SOURCE /mount HTTP/1.0`.
#[derive(Clone, Copy)]
pub struct SourceMethodLayer;

impl<S> Layer<S> for SourceMethodLayer {
    type Service = SourceMethodService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        SourceMethodService { inner }
    }
}

#[derive(Clone)]
pub struct SourceMethodService<S> {
    inner: S,
}

impl<S, ResBody> Service<Request<Body>> for SourceMethodService<S>
where
    S: Service<Request<Body>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request<Body>) -> Self::Future {
        if req.method().as_str() == "SOURCE" {
            *req.method_mut() = Method::PUT;
        }
        let fut = self.inner.call(req);
        Box::pin(fut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use tower::{ServiceBuilder, ServiceExt};

    async fn echo_method(req: Request<Body>) -> Result<Response<Body>, std::convert::Infallible> {
        let method = req.method().as_str().to_string();
        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(Body::from(method))
            .unwrap())
    }

    #[tokio::test]
    async fn source_method_is_rewritten_to_put() {
        let svc = ServiceBuilder::new()
            .layer(SourceMethodLayer)
            .service_fn(echo_method);
        let req = Request::builder()
            .method("SOURCE")
            .uri("/stream")
            .body(Body::empty())
            .unwrap();
        let resp = svc.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
        assert_eq!(&body[..], b"PUT");
    }

    #[tokio::test]
    async fn other_methods_are_unchanged() {
        for method in &["GET", "POST", "DELETE", "PUT"] {
            let svc = ServiceBuilder::new()
                .layer(SourceMethodLayer)
                .service_fn(echo_method);
            let req = Request::builder()
                .method(*method)
                .uri("/stream")
                .body(Body::empty())
                .unwrap();
            let resp = svc.oneshot(req).await.unwrap();
            let body = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
            assert_eq!(std::str::from_utf8(&body).unwrap(), *method);
        }
    }
}
