use crate::{
    auth::Credentials,
    error::Error,
    http::{
        headers::{DEFAULT_ACCEPT, DEFAULT_CONTENT_TYPE, DEFAULT_USER_AGENT, HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT},
        request::Body,
        response::Response,
        Method,
    },
};

use base64;
use base64::write::EncoderWriter as Base64Encoder;
use bytes::BytesMut;
use serde::Serialize;
use std::error;
use std::fmt;
use std::fmt::Debug;
use std::io::{self, Write};
use url::Url;

/// Error that can occur when building a [Transport]
#[derive(Debug)]
pub enum BuildError {
    /// IO error
    Io(io::Error),

    /// Certificate error
    Cert(reqwest::Error),
}

impl From<io::Error> for BuildError {
    fn from(err: io::Error) -> BuildError {
        BuildError::Io(err)
    }
}

impl From<reqwest::Error> for BuildError {
    fn from(err: reqwest::Error) -> BuildError {
        BuildError::Cert(err)
    }
}

impl error::Error for BuildError {
    fn description(&self) -> &str {
        match *self {
            BuildError::Io(ref err) => err.description(),
            BuildError::Cert(ref err) => err.description(),
        }
    }

    fn cause(&self) -> Option<&dyn error::Error> {
        match *self {
            BuildError::Io(ref err) => Some(err as &dyn error::Error),
            BuildError::Cert(ref err) => Some(err as &dyn error::Error),
        }
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            BuildError::Io(ref err) => fmt::Display::fmt(err, f),
            BuildError::Cert(ref err) => fmt::Display::fmt(err, f),
        }
    }
}

/// Default address to Elasticsearch running on `http://localhost:9200`
pub static DEFAULT_ADDRESS: &str = "http://localhost:9200";

/// Builds a HTTP transport to make API calls to Elasticsearch
pub struct TransportBuilder {
    client_builder: reqwest::ClientBuilder,
    conn_pool: Box<dyn ConnectionPool>,
    credentials: Option<Credentials>,
    proxy: Option<Url>,
    disable_proxy: bool,
}

impl TransportBuilder {
    /// Creates a new instance of [TransportBuilder]. Accepts a [ConnectionPool]
    /// from which [Connection]s to Elasticsearch will be retrieved.
    pub fn new<P>(conn_pool: P) -> Self
    where
        P: ConnectionPool + Debug + Clone + 'static,
    {
        Self {
            client_builder: reqwest::ClientBuilder::new(),
            conn_pool: Box::new(conn_pool),
            credentials: None,
            proxy: None,
            disable_proxy: false,
        }
    }

    /// Configures a proxy
    pub fn proxy(mut self, url: Url) -> Self {
        self.proxy = Some(url);
        self
    }

    /// Whether to disable proxies, including system proxies
    pub fn disable_proxy(mut self) -> Self {
        self.disable_proxy = true;
        self
    }

    /// Credentials for the client to use for authentication to Elasticsearch
    pub fn auth(mut self, credentials: Credentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// Builds a [Transport] to use to send API calls to Elasticsearch.
    pub fn build(self) -> Result<Transport, BuildError> {
        let mut client_builder = self.client_builder;

        if let Some(c) = &self.credentials {
            client_builder = match c {
                Credentials::Cert(b, p) => {
                    let pkcs12 = reqwest::Identity::from_pkcs12_der(&b, p)?;
                    client_builder.identity(pkcs12)
                }
                _ => client_builder,
            }
        };

        if self.disable_proxy {
            client_builder = client_builder.no_proxy();
        } else if let Some(url) = &self.proxy {
            client_builder = match url.scheme() {
                "https" => client_builder.proxy(reqwest::Proxy::https(&url.to_string())?),
                _ => client_builder.proxy(reqwest::Proxy::http(&url.to_string())?),
            };
        }

        let client = client_builder.build()?;
        Ok(Transport {
            client,
            conn_pool: self.conn_pool,
            credentials: self.credentials,
        })
    }
}

impl Default for TransportBuilder {
    /// Creates a default implementation using the default implementation of [SingleNodeConnectionPool].
    fn default() -> Self {
        Self::new(SingleNodeConnectionPool::default())
    }
}

/// A connection to an Elasticsearch node, used to send an API request
#[derive(Debug, Clone)]
pub struct Connection {
    url: Url,
}

impl Connection {
    /// Creates a new instance of a [Connection]
    pub fn new(url: Url) -> Self {
        Self { url }
    }
}

/// A HTTP transport responsible for making the API requests to Elasticsearch,
/// using a [Connection] selected from a [ConnectionPool]
#[derive(Debug, Clone)]
pub struct Transport {
    client: reqwest::Client,
    credentials: Option<Credentials>,
    conn_pool: Box<dyn ConnectionPool>,
}

impl Transport {
    fn method(&self, method: Method) -> reqwest::Method {
        match method {
            Method::Get => reqwest::Method::GET,
            Method::Put => reqwest::Method::PUT,
            Method::Post => reqwest::Method::POST,
            Method::Delete => reqwest::Method::DELETE,
            Method::Head => reqwest::Method::HEAD,
        }
    }

    /// Creates a new instance of a [Transport] configured with a
    /// [SingleNodeConnectionPool].
    pub fn single_node(url: &str) -> Result<Transport, Error> {
        let u = Url::parse(url)?;
        let conn_pool = SingleNodeConnectionPool::new(u);
        let transport = TransportBuilder::new(conn_pool).build()?;
        Ok(transport)
    }

    /// Creates a new instance of a [Transport] configured for use with
    /// [Elasticsearch service in Elastic Cloud](https://www.elastic.co/cloud/).
    ///
    /// * `cloud_id`: The Elastic Cloud Id retrieved from the cloud web console, that uniquely
    ///               identifies the deployment instance.
    /// * `credentials`: A set of credentials the client should use to authenticate to Elasticsearch service.
    pub fn cloud(cloud_id: &str, credentials: Credentials) -> Result<Transport, Error> {
        let conn_pool = CloudConnectionPool::new(cloud_id)?;
        let transport = TransportBuilder::new(conn_pool).auth(credentials).build()?;
        Ok(transport)
    }

    /// Creates an asynchronous request that can be awaited
    pub async fn send<B, Q>(
        &self,
        method: Method,
        path: &str,
        headers: HeaderMap,
        query_string: Option<&Q>,
        body: Option<B>,
    ) -> Result<Response, Error>
    where
        B: Body,
        Q: Serialize + ?Sized,
    {
        let connection = self.conn_pool.next();
        let url = connection.url.join(path)?;
        let reqwest_method = self.method(method);
        let mut request_builder = self.client.request(reqwest_method, &url.to_string());

        let mut headers = headers;
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(DEFAULT_CONTENT_TYPE));
        headers.insert(ACCEPT, HeaderValue::from_static(DEFAULT_ACCEPT));
        headers.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));
        request_builder = request_builder.headers(headers);

        if let Some(b) = body {
            let mut bytes_mut = BytesMut::with_capacity(1024);
            b.write(&mut bytes_mut)?;
            let bytes = bytes_mut.freeze();
            request_builder = request_builder.body(bytes);
        };

        if let Some(q) = query_string {
            request_builder = request_builder.query(q);
        }

        if let Some(c) = &self.credentials {
            request_builder = match c {
                Credentials::Basic(u, p) => request_builder.basic_auth(u, Some(p)),
                Credentials::Bearer(t) => request_builder.bearer_auth(t),
                Credentials::Cert(_, _) => request_builder,
                Credentials::ApiKey(i, k) => {
                    let mut header_value = b"ApiKey ".to_vec();
                    {
                        let mut encoder = Base64Encoder::new(&mut header_value, base64::STANDARD);
                        write!(encoder, "{}:", i).unwrap();
                        write!(encoder, "{}", k).unwrap();
                    }
                    request_builder.header(
                        AUTHORIZATION,
                        HeaderValue::from_bytes(&header_value).unwrap(),
                    )
                }
            }
        }

        let response = request_builder.send().await;
        match response {
            Ok(r) => Ok(Response::new(r)),
            Err(e) => {
                if e.is_timeout() {
                    Err(Error::Lib(format!("Request timed out to {:?}", e.url())))
                } else {
                    Err(Error::Http(e))
                }
            }
        }
    }
}

impl Default for Transport {
    fn default() -> Self {
        TransportBuilder::default().build().unwrap()
    }
}

/// A pool of [Connection]s, used to make API calls to Elasticsearch.
///
/// A [ConnectionPool] manages the connections, with different implementations determining how
/// to get the next [Connection]. The simplest type of [ConnectionPool] is [SingleNodeConnectionPool],
/// which manages only a single connection, but other implementations may manage connections more
/// dynamically at runtime, based upon the response to API calls.
pub trait ConnectionPool: Debug + objekt::Clone {
    /// Gets a reference to the next [Connection]
    fn next(&self) -> &Connection;
}

clone_trait_object!(ConnectionPool);

/// A connection pool that manages the single connection to an Elasticsearch cluster.
#[derive(Debug, Clone)]
pub struct SingleNodeConnectionPool {
    connection: Connection,
}

impl SingleNodeConnectionPool {
    /// Creates a new instance of [SingleNodeConnectionPool].
    pub fn new(url: Url) -> Self {
        Self {
            connection: Connection::new(url),
        }
    }
}

impl Default for SingleNodeConnectionPool {
    /// Creates a default instance of [SingleNodeConnectionPool], using [DEFAULT_ADDRESS].
    fn default() -> Self {
        Self::new(Url::parse(DEFAULT_ADDRESS).unwrap())
    }
}

impl ConnectionPool for SingleNodeConnectionPool {
    /// Gets a reference to the next [Connection]
    fn next(&self) -> &Connection {
        &self.connection
    }
}

/// An identifier that uniquely identifies an Elasticsearch cluster running
/// on [Elasticsearch service in Elastic Cloud](https://www.elastic.co/cloud/).
///
/// See the [Cloud Id documentation](https://www.elastic.co/guide/en/cloud/current/ec-cloud-id.html)
/// for more details.
#[derive(Debug, Clone)]
pub struct CloudId {
    /// The name of the cluster
    pub name: String,
    /// The [url::Url] to the cluster
    pub url: Url,
}

impl CloudId {
    /// Parses a [CloudId] from a string slice representation of the id retrieved from the cloud
    /// web console
    pub fn parse(cloud_id: &str) -> Result<CloudId, Error> {
        if cloud_id.is_empty() || !cloud_id.contains(':') {
            return Err(Error::Lib(
                "cloud_id should be of the form '<cluster name>:<base64 data>'".into(),
            ));
        }

        let parts: Vec<&str> = cloud_id.splitn(2, ':').collect();
        let name: String = parts[0].into();
        if name.is_empty() {
            return Err(Error::Lib("cloud_id cluster name cannot be empty".into()));
        }

        let data = parts[1];
        let decoded_result = base64::decode(data);
        if decoded_result.is_err() {
            return Err(Error::Lib(format!("cannot base 64 decode '{}'", data)));
        }

        let decoded = decoded_result.unwrap();
        let mut decoded_parts = decoded.split(|&b| b == b'$');
        let domain_name;
        let uuid;

        match decoded_parts.next() {
            Some(p) => {
                match std::str::from_utf8(p) {
                    Ok(s) => {
                        domain_name = s.trim();
                        if domain_name.is_empty() {
                            return Err(Error::Lib("decoded '<base64 data>' must contain a domain name as the first part".into()));
                        }
                    }
                    Err(_) => {
                        return Err(Error::Lib(
                            "decoded '<base64 data>' must contain a domain name as the first part"
                                .into(),
                        ))
                    }
                }
            }
            None => {
                return Err(Error::Lib(
                    "decoded '<base64 data>' must contain a domain name as the first part".into(),
                ));
            }
        }

        match decoded_parts.next() {
            Some(p) => match std::str::from_utf8(p) {
                Ok(s) => {
                    uuid = s.trim();
                    if uuid.is_empty() {
                        return Err(Error::Lib(
                            "decoded '<base64 data>' must contain a uuid as the second part".into(),
                        ));
                    }
                }
                Err(_) => {
                    return Err(Error::Lib(
                        "decoded '<base64 data>' must contain a uuid as the second part".into(),
                    ))
                }
            },
            None => {
                return Err(Error::Lib(
                    "decoded '<base64 data>' must contain at least two parts".into(),
                ));
            }
        }

        let url = Url::parse(format!("https://{}.{}", uuid, domain_name).as_ref())?;
        Ok(CloudId { name, url })
    }
}

/// A connection pool that manages the single connection to an Elasticsearch cluster running
/// on [Elasticsearch service in Elastic Cloud](https://www.elastic.co/cloud/).
#[derive(Debug, Clone)]
pub struct CloudConnectionPool {
    cloud_id: CloudId,
    connection: Connection,
}

impl CloudConnectionPool {
    /// Creates a new instance of [CloudConnectionPool].
    pub fn new(cloud_id: &str) -> Result<Self, Error> {
        let cloud = CloudId::parse(cloud_id)?;
        let connection = Connection::new(cloud.url.clone());
        Ok(Self {
            cloud_id: cloud,
            connection,
        })
    }
}

impl ConnectionPool for CloudConnectionPool {
    /// Gets a reference to the next [Connection]
    fn next(&self) -> &Connection {
        &self.connection
    }
}

#[cfg(test)]
pub mod tests {
    use crate::auth::Credentials;
    use crate::http::transport::{CloudId, TransportBuilder, SingleNodeConnectionPool};
    use url::Url;

    #[test]
    fn invalid_cert_credentials() {
        let conn_pool = SingleNodeConnectionPool::default();
        let builder = TransportBuilder::new(conn_pool)
            .auth(Credentials::Cert(b"Nonsense".to_vec(), "".into()));

        let res = builder.build();
        assert!(res.is_err());
    }

    #[test]
    fn can_parse_cloud_id() {
        let base64 = base64::encode("cloud-endpoint.example$3dadf823f05388497ea684236d918a1a$3f26e1609cf54a0f80137a80de560da4");
        let cloud_id = format!("my_cluster:{}", base64);
        let result = CloudId::parse(&cloud_id);
        assert!(result.is_ok());
        let cloud = result.unwrap();
        assert_eq!("my_cluster", cloud.name);
        assert_eq!(
            Url::parse("https://3dadf823f05388497ea684236d918a1a.cloud-endpoint.example").unwrap(),
            cloud.url
        );
    }

    #[test]
    fn cloud_id_must_contain_colon() {
        let base64 = base64::encode("cloud-endpoint.example$3dadf823f05388497ea684236d918a1a$3f26e1609cf54a0f80137a80de560da4");
        let cloud_id = format!("my_cluster{}", base64);
        let cloud = CloudId::parse(&cloud_id);
        assert!(cloud.is_err());
    }

    #[test]
    fn cloud_id_second_part_must_be_base64() {
        let cloud_id = "my_cluster:not_base_64";
        let cloud = CloudId::parse(cloud_id);
        assert!(cloud.is_err());
    }

    #[test]
    fn cloud_id_first_cannot_be_empty() {
        let base64 = base64::encode("cloud-endpoint.example$3dadf823f05388497ea684236d918a1a$3f26e1609cf54a0f80137a80de560da4");
        let cloud_id = format!(":{}", base64);
        let cloud = CloudId::parse(&cloud_id);
        assert!(cloud.is_err());
    }

    #[test]
    fn cloud_id_second_part_cannot_be_empty() {
        let cloud_id = "cluster_name:";
        let cloud = CloudId::parse(&cloud_id);
        assert!(cloud.is_err());
    }

    #[test]
    fn cloud_id_second_part_must_have_at_least_two_parts() {
        let base64 = base64::encode("cloud-endpoint.example");
        let cloud_id = format!("my_cluster:{}", base64);
        let result = CloudId::parse(&cloud_id);
        assert!(result.is_err());
    }
}