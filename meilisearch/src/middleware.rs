//! Contains all the custom middleware used in meilisearch

use std::future::{ready, Ready};

use actix_web::dev::{self, Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::web::Data;
use actix_web::Error;
use futures_util::future::LocalBoxFuture;
use index_scheduler::IndexScheduler;
use prometheus::HistogramTimer;

pub struct RouteMetrics;

// Middleware factory is `Transform` trait from actix-service crate
// `S` - type of the next service
// `B` - type of response's body
impl<S, B> Transform<S, ServiceRequest> for RouteMetrics
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = RouteMetricsMiddleware<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(RouteMetricsMiddleware { service }))
    }
}

pub struct RouteMetricsMiddleware<S> {
    service: S,
}

impl<S, B> Service<ServiceRequest> for RouteMetricsMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    dev::forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        let mut histogram_timer: Option<HistogramTimer> = None;

        // calling unwrap here is safe because index scheduler is added to app data while creating actix app.
        // also, the tests will fail if this is not present.
        let index_scheduler = req.app_data::<Data<IndexScheduler>>().unwrap();
        let features = index_scheduler.features();

        if features.check_metrics().is_ok() {
            let request_path = req.path();
            let generic_route = req.resource_map().match_pattern(request_path);

            // We group certain routes together as we otherwise would generate too many
            // prometheus metric with no additional value.
            // See: https://github.com/meilisearch/meilisearch/issues/3983
            let opt_metric_path = match generic_route.as_deref() {
                Some("/tasks/{task_id}") => Some("/tasks/{task_id}"),
                Some(x) if x.starts_with("/keys") => None,
                Some(x)
                    if x.starts_with("/fonts/")
                        || x.starts_with("/static/")
                        || x.starts_with("/favicon")
                        || x == "/manifest.json" =>
                {
                    Some("static_resource")
                }
                other => other,
            };

            if let Some(metric_group) = opt_metric_path {
                let request_method = req.method().to_string();
                histogram_timer = Some(
                    crate::metrics::MEILISEARCH_HTTP_RESPONSE_TIME_SECONDS
                        .with_label_values(&[&request_method, metric_group])
                        .start_timer(),
                );
                crate::metrics::MEILISEARCH_HTTP_REQUESTS_TOTAL
                    .with_label_values(&[&request_method, metric_group])
                    .inc();
            }
        };

        let fut = self.service.call(req);

        Box::pin(async move {
            let res = fut.await?;

            if let Some(histogram_timer) = histogram_timer {
                histogram_timer.observe_duration();
            };
            Ok(res)
        })
    }
}
