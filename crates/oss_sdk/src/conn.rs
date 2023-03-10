use std::borrow::Cow;
use std::net::Ipv4Addr;
use std::sync::Arc;

use once_cell::sync::OnceCell;
use reqsign::AliyunOssBuilder;
use reqsign::AliyunOssSigner;
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{Request, Url};

use crate::config::ClientConfig;
use crate::error::{OSSError, ServiceError};
use crate::types::{Credentials, Headers, Params};
use crate::util;
use crate::Result;

#[derive(Debug, Clone)]
pub(crate) struct Conn {
    config: Arc<ClientConfig>,
    url_maker: Arc<UrlMaker>,
    client: reqwest::Client,
}

impl Conn {
    pub(crate) fn new(
        config: Arc<ClientConfig>,
        url_maker: Arc<UrlMaker>,
        client: reqwest::Client,
    ) -> Conn {
        Conn {
            config,
            url_maker,
            client,
        }
    }

    fn get_signer(&self, bucket: &str) -> &AliyunOssSigner {
        static INSTANCE: OnceCell<AliyunOssSigner> = OnceCell::new();
        INSTANCE.get_or_init(|| {
            let mut builder = AliyunOssBuilder::default();
            builder.access_key_id(&self.config.access_key_id);
            builder.access_key_secret(&self.config.access_key_secret);
            builder.bucket(bucket);
            builder.build().unwrap()
        })
    }

    pub(crate) async fn execute(
        &self,
        method: reqwest::Method,
        bucket: &str,
        object: &str,
        params: Option<Params>,
        headers: Option<Headers>,
        data: Vec<u8>,
        init_crc: u64,
    ) -> Result<Vec<u8>> {
        let url_params = match params {
            Some(ref it) => Some(Self::get_url_params(it)?),
            None => None,
        };

        let url = self
            .url_maker
            .to_uri(bucket, object, &url_params.unwrap_or_default());

        let signer = self.get_signer(bucket);

        // let mut req = Request {
        //     url,
        //     method,
        //     headers: headers.unwrap_or_default(),
        //     params: params.unwrap_or_default(),
        //     body: data,
        // };

        let mut req = Request::new(method, url);

        // handle headers
        if let Some(headers) = headers {
            for (k, v) in &headers {
                req.headers_mut()
                    .insert(HeaderName::try_from(k)?, HeaderValue::try_from(v)?);
            }
        }

        // handle body
        if !data.is_empty() && self.config.enable_md5 {
            // TODO: md5 threshold
            let md5sum = format!("{}", base64::encode(md5::compute(&data).0));
            req.headers_mut().insert("content-md5", md5sum.parse()?);
        }

        if !data.is_empty() && self.config.enable_crc {
            // TODO: crc
        }

        // TODO: http proxy

        // // http time
        // let date = util::httptime();
        // req.headers.insert("date".into(), date);

        // user-agent
        req.headers_mut()
            .insert("user-agent", self.config.ua.clone().parse()?);

        // // host
        // if let Ok(it) = Url::parse(&req.url) {
        //     if let Some(host) = it.host_str() {
        //         req.headers.insert("host".into(), host.into());
        //     }
        // }

        let token = self.config.security_token();
        if !token.is_empty() {
            req.headers_mut()
                .insert("x-oss-security-token", token.parse()?);
        }

        //TODO: sign
        signer.sign(&mut req).expect("sign request must success");

        let resp = self.client.execute(req.try_into()?).await?;
        // let resp = req.send(&self.client).await?;
        let status_code = resp.status().as_u16();
        let is_success = resp.status().is_success();
        let b = resp.bytes().await?.to_vec();

        if is_success {
            Ok(b)
        } else {
            if let Ok(e) = ServiceError::try_from_xml(&b) {
                Err(OSSError::ServiceError(status_code, e.code, e.message, e.request_id).into())
            } else {
                bail!("{}", String::from_utf8_lossy(&b))
            }
        }
    }

    fn get_url_params(params: &Params) -> Result<String> {
        let s = serde_urlencoded::to_string(params)?;
        Ok(s.replace("+", "%20"))
    }
}

#[derive(Debug, Clone)]
pub(crate) enum UrlType {
    CNAME,
    IP,
    ALIYUN,
}

#[derive(Debug, Clone)]
pub(crate) struct UrlMaker {
    schema: String,
    net_loc: String,
    typ: UrlType,
    is_proxy: bool,
}

impl UrlMaker {
    pub(crate) fn new(endpoint: &str, is_cname: bool, is_proxy: bool) -> Result<UrlMaker> {
        let url = match Url::parse(endpoint) {
            Ok(u) => u,
            Err(_) => Url::parse(&format!("http://{}", endpoint))?,
        };
        let schema = url.scheme();

        match schema {
            "http" | "https" => match url.host_str() {
                Some(host) => {
                    let typ = match host.parse::<Ipv4Addr>() {
                        Ok(add) => UrlType::IP,
                        _ => {
                            if is_cname {
                                UrlType::CNAME
                            } else {
                                UrlType::ALIYUN
                            }
                        }
                    };

                    Ok(UrlMaker {
                        is_proxy,
                        schema: schema.into(),
                        net_loc: host.into(),
                        typ,
                    })
                }
                None => bail!("cannot extract host info from endpoint '{}'!", endpoint),
            },
            _ => bail!("invalid schema {}: should be http or https only!", schema),
        }
    }

    pub(crate) fn to_uri(&self, bucket: &str, object: &str, params: &str) -> Url {
        let uri = self.get_url(bucket, object, params);
        Url::parse(&uri).unwrap()
    }

    pub(crate) fn get_url(&self, bucket: &str, object: &str, params: &str) -> String {
        let (host, path) = self.build_url(bucket, object);
        if params.is_empty() {
            format!("{}://{}{}", self.schema, host, path)
        } else {
            format!("{}://{}{}?{}", self.schema, host, path, params)
        }
    }

    // build to (host,path)
    fn build_url(&self, bucket: &str, object: &str) -> (Cow<str>, Cow<str>) {
        let object = util::query_escape(object);
        match self.typ {
            UrlType::CNAME => {
                let host = Cow::from(&self.net_loc[..]);
                let path = Cow::from(format!("/{}", object));
                (host, path)
            }
            UrlType::IP => {
                let host = Cow::from(&self.net_loc[..]);
                let path = if bucket.is_empty() {
                    Cow::from("/")
                } else {
                    Cow::from(format!("/{}/{}", bucket, object))
                };
                (host, path)
            }
            UrlType::ALIYUN => {
                if bucket.is_empty() {
                    let host = Cow::from(&self.net_loc[..]);
                    let path = Cow::from("/");
                    (host, path)
                } else {
                    let host = Cow::from(format!("{}.{}", bucket, self.net_loc));
                    let path = Cow::from(format!("/{}", object));
                    (host, path)
                }
            }
        }
    }
}
