#![allow(missing_docs)]

//! A basic API client for interacting with the Kubernetes API
//!
//! The [`Client`] uses standard kube error handling.
//!
//! This client can be used on its own or in conjuction with the [`Api`][crate::api::Api]
//! type for more structured interaction with the kubernetes API.
//!
//! The [`Client`] can also be used with [`Discovery`](crate::Discovery) to dynamically
//! retrieve the resources served by the kubernetes API.

use std::convert::TryFrom;

use bytes::Bytes;
use either::{Either, Left, Right};
use futures::{self, Stream, TryStream};
use http::{self, Request, Response, StatusCode};
use k8s_openapi::apimachinery::pkg::apis::meta::v1 as k8s_meta_v1;
pub use kube_core::response::Status;
use serde::de::DeserializeOwned;
use serde_json::{self, Value};
use futures::StreamExt;

use crate::{api::WatchEvent, error::ErrorResponse, Config, Error, Result};

/// Client for connecting with a Kubernetes cluster.
///
/// The easiest way to instantiate the client is either by
/// inferring the configuration from the environment using
/// [`Client::try_default`] or with an existing [`Config`]
/// using [`Client::try_from`].
#[cfg_attr(docsrs, doc(cfg(feature = "client-wasi")))]
#[derive(Clone)]
pub struct Client {
    // - `Buffer` for cheap clone
    // - `BoxService` for dynamic response future type
    default_ns: String,
}

impl Client {
    /// Create a [`Client`] using a custom `Service` stack.
    ///
    /// [`ConfigExt`](crate::client::ConfigExt) provides extensions for
    /// building a custom stack.
    ///
    /// To create with the default stack with a [`Config`], use
    /// [`Client::try_from`].
    ///
    /// To create with the default stack with an inferred [`Config`], use
    /// [`Client::try_default`].
    ///
    /// # Example
    ///
    /// ```rust
    /// # async fn doc() -> Result<(), Box<dyn std::error::Error>> {
    /// use kube::{client::ConfigExt, Client, Config};
    /// use tower::ServiceBuilder;
    ///
    /// let config = Config::infer().await?;
    /// let service = ServiceBuilder::new()
    ///     .layer(config.base_uri_layer())
    ///     .option_layer(config.auth_layer()?)
    ///     .service(hyper::Client::new());
    /// let client = Client::new(service, config.default_namespace);
    /// # Ok(())
    /// # }
    /// ```
    pub fn new<T>(default_namespace: T) -> Self
    where
        T: Into<String>,
    {
        Self {
            default_ns: default_namespace.into(),
        }
    }

    /// Create and initialize a [`Client`] using the inferred configuration.
    ///
    /// Will use [`Config::infer`] which attempts to load the local kubec-config first,
    /// and then if that fails, trying the in-cluster environment variables.
    ///
    /// Will fail if neither configuration could be loaded.
    ///
    /// If you already have a [`Config`] then use [`Client::try_from`](Self::try_from)
    /// instead.
    pub async fn try_default() -> Result<Self> {
        Ok(Self::new("default"))
    }

    pub(crate) fn default_ns(&self) -> &str {
        &self.default_ns
    }

    async fn send(&self, request: Request<Vec<u8>>) -> Result<Response<Vec<u8>>> {
        Ok(kube_runtime_abi::execute_request(request).await)
    }

    /// Perform a raw HTTP request against the API and deserialize the response
    /// as JSON to some known type.
    pub async fn request<T>(&self, request: Request<Vec<u8>>) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let text = self.request_text(request).await?;

        serde_json::from_str(&text).map_err(|e| {
            tracing::warn!("{}, {:?}", text, e);
            Error::SerdeError(e)
        })
    }

    /// Perform a raw HTTP request against the API and get back the response
    /// as a string
    pub async fn request_text(&self, request: Request<Vec<u8>>) -> Result<String> {
        let res = self.send(request).await?;
        let status = res.status();
        // trace!("Status = {:?} for {}", status, res.url());
        let body_bytes = res.into_body();
        let text = String::from_utf8(body_bytes).map_err(Error::FromUtf8)?;
        handle_api_errors(&text, status)?;

        Ok(text)
    }

    /// Perform a raw HTTP request against the API and get back the response
    /// as a stream of bytes
    pub async fn request_text_stream(
        &self,
        request: Request<Vec<u8>>,
    ) -> Result<impl Stream<Item = Result<Bytes>>> {
        let response = kube_runtime_abi::execute_request_stream(request).await;

        Ok(response.into_body().map(|line| Ok(Bytes::from(line))))
    }

    /// Perform a raw HTTP request against the API and get back either an object
    /// deserialized as JSON or a [`Status`] Object.
    pub async fn request_status<T>(&self, request: Request<Vec<u8>>) -> Result<Either<T, Status>>
    where
        T: DeserializeOwned,
    {
        let text = self.request_text(request).await?;
        // It needs to be JSON:
        let v: Value = serde_json::from_str(&text).map_err(Error::SerdeError)?;
        if v["kind"] == "Status" {
            tracing::trace!("Status from {}", text);
            Ok(Right(serde_json::from_str::<Status>(&text).map_err(|e| {
                tracing::warn!("{}, {:?}", text, e);
                Error::SerdeError(e)
            })?))
        } else {
            Ok(Left(serde_json::from_str::<T>(&text).map_err(|e| {
                tracing::warn!("{}, {:?}", text, e);
                Error::SerdeError(e)
            })?))
        }
    }

    /// Perform a raw request and get back a stream of [`WatchEvent`] objects
    pub async fn request_events<T>(
        &self,
        request: Request<Vec<u8>>,
    ) -> Result<impl TryStream<Item = Result<WatchEvent<T>>>>
    where
        T: Clone + DeserializeOwned,
    {
        let response = kube_runtime_abi::execute_request_stream(request).await;
        
        Ok(response.into_body().filter_map(|line| async move {
            let line = match String::from_utf8(line).map_err(Error::FromUtf8) {
                Ok(line) => line,
                Err(err) => return Some(Err(err)),
            };

            match serde_json::from_str::<WatchEvent<T>>(&line) {
                Ok(event) => Some(Ok(event)),
                Err(e) => {
                    // Ignore EOF error that can happen for incomplete line from `decode_eof`.
                    if e.is_eof() {
                        return None;
                    }
    
                    // Got general error response
                    if let Ok(e_resp) = serde_json::from_str::<ErrorResponse>(&line) {
                        return Some(Err(Error::Api(e_resp)));
                    }
                    // Parsing error
                    Some(Err(Error::SerdeError(e)))
                }
            }
        }))
    }
}

/// Low level discovery methods using `k8s_openapi` types.
///
/// Consider using the [`discovery`](crate::discovery) module for
/// easier-to-use variants of this functionality.
/// The following methods might be deprecated to avoid confusion between similarly named types within `discovery`.
impl Client {
    /// Returns apiserver version.
    pub async fn apiserver_version(&self) -> Result<k8s_openapi::apimachinery::pkg::version::Info> {
        self.request(
            Request::builder()
                .uri("/version")
                .body(vec![])
                .map_err(Error::HttpError)?,
        )
        .await
    }

    /// Lists api groups that apiserver serves.
    pub async fn list_api_groups(&self) -> Result<k8s_meta_v1::APIGroupList> {
        self.request(
            Request::builder()
                .uri("/apis")
                .body(vec![])
                .map_err(Error::HttpError)?,
        )
        .await
    }

    /// Lists resources served in given API group.
    ///
    /// ### Example usage:
    /// ```rust
    /// # async fn scope(client: kube::Client) -> Result<(), Box<dyn std::error::Error>> {
    /// let apigroups = client.list_api_groups().await?;
    /// for g in apigroups.groups {
    ///     let ver = g
    ///         .preferred_version
    ///         .as_ref()
    ///         .or_else(|| g.versions.first())
    ///         .expect("preferred or versions exists");
    ///     let apis = client.list_api_group_resources(&ver.group_version).await?;
    ///     dbg!(apis);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_api_group_resources(&self, apiversion: &str) -> Result<k8s_meta_v1::APIResourceList> {
        let url = format!("/apis/{}", apiversion);
        self.request(
            Request::builder()
                .uri(url)
                .body(vec![])
                .map_err(Error::HttpError)?,
        )
        .await
    }

    /// Lists versions of `core` a.k.a. `""` legacy API group.
    pub async fn list_core_api_versions(&self) -> Result<k8s_meta_v1::APIVersions> {
        self.request(
            Request::builder()
                .uri("/api")
                .body(vec![])
                .map_err(Error::HttpError)?,
        )
        .await
    }

    /// Lists resources served in particular `core` group version.
    pub async fn list_core_api_resources(&self, version: &str) -> Result<k8s_meta_v1::APIResourceList> {
        let url = format!("/api/{}", version);
        self.request(
            Request::builder()
                .uri(url)
                .body(vec![])
                .map_err(Error::HttpError)?,
        )
        .await
    }
}

/// Kubernetes returned error handling
///
/// Either kube returned an explicit ApiError struct,
/// or it someohow returned something we couldn't parse as one.
///
/// In either case, present an ApiError upstream.
/// The latter is probably a bug if encountered.
fn handle_api_errors(text: &str, s: StatusCode) -> Result<()> {
    if s.is_client_error() || s.is_server_error() {
        // Print better debug when things do fail
        // trace!("Parsing error: {}", text);
        if let Ok(errdata) = serde_json::from_str::<ErrorResponse>(text) {
            tracing::debug!("Unsuccessful: {:?}", errdata);
            Err(Error::Api(errdata))
        } else {
            tracing::warn!("Unsuccessful data error parse: {}", text);
            let ae = ErrorResponse {
                status: s.to_string(),
                code: s.as_u16(),
                message: format!("{:?}", text),
                reason: "Failed to parse error data".into(),
            };
            tracing::debug!("Unsuccessful: {:?} (reconstruct)", ae);
            Err(Error::Api(ae))
        }
    } else {
        Ok(())
    }
}

impl TryFrom<Config> for Client {
    type Error = Error;

    /// Convert [`Config`] into a [`Client`]
    fn try_from(config: Config) -> Result<Self> {
        let default_ns = config.default_namespace.clone();

        Ok(Self::new(default_ns))
    }
}
