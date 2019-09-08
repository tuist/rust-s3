extern crate base64;
extern crate md5;

use snafu::{ResultExt, Snafu};

use std::collections::HashMap;
use std::io::{Read, Write};

use bucket::Bucket;
use chrono::{DateTime, Utc};
use command::Command;
use hmac::{Hmac, Mac};
use reqwest::async as async;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use sha2::{Digest, Sha256};
use hex::ToHex;
use url::Url;

use futures::prelude::*;

use serde_types::AwsError;
use signing;

use EMPTY_PAYLOAD_SHA;
use LONG_DATE;
use reqwest::async::Response;
use core::fmt;
use std::error;

#[derive(Debug, Default)]
pub struct S3Error {}

impl fmt::Display for S3Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "S3Error!")
    }
}

impl error::Error for S3Error {
    fn description(&self) -> &str {
        "Description for S3Error"
    }

    fn cause(&self) -> Option<&error::Error> {
        None
    }
}

#[derive(Debug, Snafu)]
pub enum Error {
    InvalidHeaderName {
        source: reqwest::header::InvalidHeaderName
    },
    InvalidHeaderValue {
        source: reqwest::header::InvalidHeaderValue
    },
    ReqwestFuture,
    ResponseError,
    ParseError {
        source: std::string::ParseError
    }

}

type S3Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Snafu)]
enum InternalError {
    BucketError {
        source: ::bucket::Error
    },
    RegionError {
        source: ::region::Error
    }
}

/// Collection of HTTP headers sent to S3 service, in key/value format.
pub type Headers = HashMap<String, String>;

/// Collection of HTTP query parameters sent to S3 service, in key/value
/// format.
pub type Query = HashMap<String, String>;


// Temporary structure for making a request
pub struct Request<'a> {
    pub bucket: &'a Bucket,
    pub path: &'a str,
    pub command: Command<'a>,
    pub datetime: DateTime<Utc>,
    pub async: bool,
}

impl<'a> Request<'a> {
    pub fn new<'b>(bucket: &'b Bucket, path: &'b str, command: Command<'b>) -> Request<'b> {
        Request {
            bucket,
            path,
            command,
            datetime: Utc::now(),
            async: false,
        }
    }

    fn url(&self) -> Url {
        let mut url_str = match self.command {
            Command::GetBucketLocation => format!("{}://{}", self.bucket.scheme(), self.bucket.self_host()),
            _ => format!("{}://{}", self.bucket.scheme(), self.bucket.host())
        };
        match self.command {
            Command::GetBucketLocation => {}
            _ => {
                url_str.push_str("/");
                url_str.push_str(&self.bucket.name());
            }
        }
        if !self.path.starts_with('/') {
            url_str.push_str("/");
        }
        match self.command {
            Command::GetBucketLocation => url_str.push_str(self.path),
            _ => url_str.push_str(&signing::uri_encode(self.path, false))
        };

        // Since every part of this URL is either pre-encoded or statically
        // generated, there's really no way this should fail.
        let mut url = Url::parse(&url_str).expect("static URL parsing");

        for (key, value) in &self.bucket.extra_query {
            url.query_pairs_mut().append_pair(key, value);
        }

        if let Command::ListBucket { prefix, delimiter, continuation_token } = self.command.clone() {
            let mut query_pairs = url.query_pairs_mut();
            delimiter.map(|d| query_pairs.append_pair("delimiter", &d.clone()));
            query_pairs.append_pair("prefix", &prefix);
            query_pairs.append_pair("list-type", "2");
            if let Some(token) = continuation_token {
                query_pairs.append_pair("continuation-token", &token);
            }
        }

        match self.command {
            Command::PutObjectTagging { .. } | Command::GetObjectTagging | Command::DeleteObjectTagging => {
                url.query_pairs_mut().append_pair("tagging", "");
            }
            _ => {}
        }

//        println!("{}", url);
        url
    }

    fn content_length(&self) -> usize {
        match self.command {
            Command::PutObject { content, .. } => content.len(),
            Command::PutObjectTagging { tags } => tags.len(),
            _ => 0,
        }
    }

    fn content_type(&self) -> String {
        match self.command {
            Command::PutObject { content_type, .. } => content_type.into(),
            _ => "text/plain".into(),
        }
    }

    fn sha256(&self) -> String {
        match self.command {
            Command::PutObject { content, .. } => {
                let mut sha = Sha256::default();
                sha.input(content);
                sha.result().as_slice().to_hex()
            }
            Command::PutObjectTagging { tags } => {
                let mut sha = Sha256::default();
                sha.input(tags.as_bytes());
                sha.result().as_slice().to_hex()
            }
            _ => EMPTY_PAYLOAD_SHA.into(),
        }
    }

    fn long_date(&self) -> String {
        self.datetime.format(LONG_DATE).to_string()
    }

    fn canonical_request(&self, headers: &HeaderMap) -> String {
        signing::canonical_request(
            self.command.http_verb().as_str(),
            &self.url(),
            headers,
            &self.sha256(),
        )
    }

    fn string_to_sign(&self, request: &str) -> String {
        signing::string_to_sign(&self.datetime, &self.bucket.region(), request)
    }

    fn signing_key(&self) -> Vec<u8> {
        signing::signing_key(
            &self.datetime,
            &self.bucket.secret_key(),
            &self.bucket.region(),
            "s3",
        )
    }

    fn authorization(&self, headers: &HeaderMap) -> String {
        let canonical_request = self.canonical_request(headers);
        let string_to_sign = self.string_to_sign(&canonical_request);
        let mut hmac = Hmac::<Sha256>::new(&self.signing_key());
        hmac.input(string_to_sign.as_bytes());
        let signature = hmac.result().code().to_hex();
        let signed_header = signing::signed_header_string(headers);
        signing::authorization_header(
            &self.bucket.access_key(),
            &self.datetime,
            &self.bucket.region(),
            &signed_header,
            &signature,
        )
    }

    fn headers(&self) -> S3Result<HeaderMap> {
        // Generate this once, but it's used in more than one place.
        let sha256 = self.sha256();

        // Start with extra_headers, that way our headers replace anything with
        // the same name.
        let mut headers = self
            .bucket
            .extra_headers
            .iter()
            .map(|(k, v)| Ok((k.parse::<HeaderName>().context(InvalidHeaderName)?, v.parse::<HeaderValue>().context(InvalidHeaderValue)?)))
            .collect::<Result<HeaderMap, Error>>()?;
        match self.command {
            Command::GetBucketLocation => headers.insert(header::HOST, self.bucket.self_host().parse().context(InvalidHeaderValue)?),
            _ => headers.insert(header::HOST, self.bucket.host().parse().context(InvalidHeaderValue)?)
        };
        headers.insert(
            header::CONTENT_LENGTH,
            self.content_length().to_string().parse().context(InvalidHeaderValue)?,
        );
        headers.insert(header::CONTENT_TYPE, self.content_type().parse().context(InvalidHeaderValue)?);
        headers.insert("X-Amz-Content-Sha256", sha256.parse().context(InvalidHeaderValue)?);
        headers.insert("X-Amz-Date", self.long_date().parse().context(InvalidHeaderValue)?);

        if let Some(token) = self.bucket.credentials().token.as_ref() {
            headers.insert("X-Amz-Security-Token", token.parse().context(InvalidHeaderValue)?);
        }

        if let Command::PutObjectTagging { tags } = self.command {
            let digest = md5::compute(tags);
            let hash = base64::encode(digest.as_ref());
            headers.insert("Content-MD5", hash.parse().context(InvalidHeaderValue)?);
        }

        // This must be last, as it signs the other headers
        let authorization = self.authorization(&headers);
        headers.insert(header::AUTHORIZATION, authorization.parse().context(InvalidHeaderValue)?);

        // The format of RFC2822 is somewhat malleable, so including it in
        // signed headers can cause signature mismatches. We do include the
        // X-Amz-Date header, so requests are still properly limited to a date
        // range and can't be used again e.g. reply attacks. Adding this header
        // after the generation of the Authorization header leaves it out of
        // the signed headers.
        headers.insert(header::DATE, self.datetime.to_rfc2822().parse().context(InvalidHeaderValue)?);

        Ok(headers)
    }

    pub fn response_data(&self) -> S3Result<(Vec<u8>, u16)> {
        match self.response_data_future().wait() {
            Ok((response_data, status_code)) => Ok((response_data, status_code)),
            Err(_) => Err(Error::ReqwestFuture)
        }
    }

    pub fn response_data_to_writer<T: Write>(&self, writer: &mut T) -> S3Result<u16> {
        match self.response_data_to_writer_future(writer).wait() {
            Ok(status_code) => Ok(status_code),
            Err(_) => Err(Error::ReqwestFuture)
        }
    }

    pub fn response_future(&self) -> impl Future<Item=Response, Error=S3Error> {
        let client = if cfg!(feature = "no-verify-ssl") {
            async::Client::builder()
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true)
                .build().expect("Could not build dangereous client!")
        } else {
            async::Client::new()
        };

        // Build headers
        let headers = self.headers().expect("Could not get headers!");

        // Get owned content to pass to reqwest
        let content = if let Command::PutObject { content, .. } = self.command {
            Vec::from(content)
        } else if let Command::PutObjectTagging { tags } = self.command {
            Vec::from(tags)
        } else {
            Vec::new()
        };

        let request = client
            .request(self.command.http_verb(), self.url())
            .headers(headers.to_owned())
            .body(content.to_owned());

        request.send().map_err(|_| S3Error {})
    }

    pub fn response_data_future(&self) -> impl Future<Item=(Vec<u8>, u16), Error=S3Error> {
        self.response_future()
            .and_then(|mut response| Ok((response.text(), response.status().as_u16()))).map_err(|_| S3Error {})
            .and_then(|(body_future, status_code)| {
                body_future.and_then(move |body| Ok((body.as_bytes().to_vec(), status_code))).map_err(|_| S3Error {})
            })
    }

    pub fn response_data_to_writer_future<'b, T: Write>(&self, writer: &'b mut T) -> impl Future<Item=u16> + 'b {
        let future_response = self.response_data_future();
        future_response.and_then(move |(body, status_code)| {
            writer.write_all(body.as_slice()).expect("Could not write to writer");
            Ok(status_code)
        })
    }

}

#[cfg(test)]
mod tests {
    use bucket::Bucket;
    use command::Command;
    use credentials::Credentials;
    use request::{Request, InternalError};
    use request::S3Result;
    use snafu::ResultExt;
    use url::form_urlencoded::Parse;
    use request::{ParseError, BucketError, RegionError};

    // Fake keys - otherwise using Credentials::default will use actual user
    // credentials if they exist.
    fn fake_credentials() -> Credentials {
        const ACCESS_KEY: &'static str = "AKIAIOSFODNN7EXAMPLE";
        const SECRET_KEY: &'static str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        Credentials::new(Some(ACCESS_KEY.into()), Some(SECRET_KEY.into()), None, None)
    }

    #[test]
    fn url_uses_https_by_default() -> Result<(), InternalError> {
        let region = "custom-region".parse().context(RegionError)?;
        let bucket = Bucket::new("my-first-bucket", region, fake_credentials()).context(BucketError)?;
        let path = "/my-first/path";
        let request = Request::new(&bucket, path, Command::GetObject);

        assert_eq!(request.url().scheme(), "https");

        let headers = request.headers().unwrap();
        let host = headers.get("Host").unwrap();

        assert_eq!(*host, "custom-region".to_string());
        Ok(())
    }

    #[test]
    fn url_uses_scheme_from_custom_region_if_defined() -> Result<(), InternalError> {
        let region = "http://custom-region".parse().context(RegionError)?;
        let bucket = Bucket::new("my-second-bucket", region, fake_credentials()).context(BucketError)?;
        let path = "/my-second/path";
        let request = Request::new(&bucket, path, Command::GetObject);

        assert_eq!(request.url().scheme(), "http");

        let headers = request.headers().unwrap();
        let host = headers.get("Host").unwrap();

        assert_eq!(*host, "custom-region".to_string());
        Ok(())
    }
}
