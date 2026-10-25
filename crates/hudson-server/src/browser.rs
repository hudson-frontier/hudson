//! Admit only explicit loopback authorities and same-origin browser requests.
use axum::{
    extract::Request,
    http::{header, HeaderName, StatusCode},
    middleware::Next,
    response::Response,
};

fn authority(value: &str) -> Option<(&'static str, u16)> {
    let value = value.to_ascii_lowercase();
    for host in ["localhost", "127.0.0.1", "[::1]"] {
        let Some(suffix) = value.strip_prefix(host) else {
            continue;
        };
        if suffix.is_empty() {
            return Some((host, 80));
        }
        let port = suffix.strip_prefix(':')?;
        if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let port = port.parse::<u16>().ok()?;
        return (port != 0).then_some((host, port));
    }
    None
}

fn single_header(request: &Request, name: HeaderName) -> Result<Option<&str>, ()> {
    let mut values = request.headers().get_all(name).iter();
    let value = values
        .next()
        .map(|value| value.to_str())
        .transpose()
        .map_err(|_| ())?;
    if values.next().is_some() {
        return Err(());
    }
    Ok(value)
}

fn trusted(request: &Request) -> Option<()> {
    if request
        .uri()
        .scheme_str()
        .is_some_and(|scheme| scheme != "http")
    {
        return None;
    }
    let host = match single_header(request, header::HOST).ok()? {
        Some(value) => Some(authority(value)?),
        None => None,
    };
    let uri = match request.uri().authority() {
        Some(value) => Some(authority(value.as_str())?),
        None => None,
    };
    let destination = host.or(uri)?;
    if host.is_some() && uri.is_some() && host != uri {
        return None;
    }
    if let Some(origin) = single_header(request, header::ORIGIN).ok()? {
        if authority(origin.strip_prefix("http://")?)? != destination {
            return None;
        }
    }
    Some(())
}

pub async fn protect(request: Request, next: Next) -> Result<Response, StatusCode> {
    if trusted(&request).is_none() {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, routing::get, Router};
    use tower::ServiceExt;

    #[tokio::test]
    async fn uri_authority_is_checked_with_or_without_a_host_header() {
        let router = Router::new()
            .route("/health", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(protect));
        for (uri, host, expected) in [
            ("http://localhost:4318/health", None, StatusCode::OK),
            (
                "http://attacker.invalid:4318/health",
                None,
                StatusCode::FORBIDDEN,
            ),
            (
                "http://localhost:4318/health",
                Some("127.0.0.1:4318"),
                StatusCode::FORBIDDEN,
            ),
            (
                "http://attacker.invalid:4318/health",
                Some("localhost:4318"),
                StatusCode::FORBIDDEN,
            ),
        ] {
            let mut request = Request::builder()
                .version(axum::http::Version::HTTP_2)
                .uri(uri);
            if let Some(host) = host {
                request = request.header(header::HOST, host);
            }
            let response = router
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
    }
}
