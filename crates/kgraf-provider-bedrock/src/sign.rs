//! SigV4 request signing over `aws-sigv4`.

use std::time::SystemTime;

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;

use crate::ADAPTER;
use crate::credentials::AwsAuth;
use kgraf_provider::ProviderError;

/// The signing name of the Bedrock runtime service.
pub const SERVICE: &str = "bedrock";

/// Sign one request and return the headers to add: `authorization`, `x-amz-date`
/// and, for temporary keys, `x-amz-security-token`.
///
/// `url` must already be percent-encoded as it will be sent. The signer encodes the
/// path a second time for the canonical request, which is the rule for every AWS
/// service except S3, so a model id carrying `:` or an ARN carrying `/` signs
/// correctly when it is sent as `%3A` and `%2F`.
pub fn sign_request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    auth: &AwsAuth,
    service: &str,
    time: SystemTime,
) -> Result<Vec<(String, String)>, ProviderError> {
    let fail = |reason: String| ProviderError::Decode {
        adapter: ADAPTER,
        reason: format!("cannot sign the request: {reason}"),
    };
    let credentials = Credentials::new(
        auth.access_key_id.clone(),
        auth.secret_access_key.clone(),
        auth.session_token.clone(),
        None,
        "kgraf",
    );
    let identity = credentials.into();
    let params = v4::SigningParams::builder()
        .identity(&identity)
        .region(&auth.region)
        .name(service)
        .time(time)
        .settings(SigningSettings::default())
        .build()
        .map_err(|e| fail(e.to_string()))?
        .into();
    let signable = SignableRequest::new(
        method,
        url,
        headers.iter().copied(),
        SignableBody::Bytes(body),
    )
    .map_err(|e| fail(e.to_string()))?;
    let (instructions, _signature) = sign(signable, &params)
        .map_err(|e| fail(e.to_string()))?
        .into_parts();
    Ok(instructions
        .headers()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn example_auth() -> AwsAuth {
        AwsAuth {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
            region: "us-east-1".into(),
            bearer_token: None,
        }
    }

    /// The published `get-vanilla` case of the AWS Signature Version 4 test suite
    /// (service `service`, region `us-east-1`, 2015-08-30T12:36:00Z):
    /// <https://docs.aws.amazon.com/general/latest/gr/signature-v4-test-suite.html>
    #[test]
    fn the_published_get_vanilla_vector_signs_to_the_published_signature() {
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_440_938_160);
        let headers = sign_request(
            "GET",
            "https://example.amazonaws.com/",
            &[],
            b"",
            &example_auth(),
            "service",
            time,
        )
        .expect("signed");
        let get = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("no {name} header in {headers:?}"))
        };
        assert_eq!(get("x-amz-date"), "20150830T123600Z");
        assert_eq!(
            get("authorization"),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    #[test]
    fn temporary_keys_add_the_security_token_header() {
        let mut auth = example_auth();
        auth.session_token = Some("session-token-value".into());
        let headers = sign_request(
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse",
            &[("content-type", "application/json")],
            b"{}",
            &auth,
            SERVICE,
            SystemTime::now(),
        )
        .expect("signed");
        let token = headers
            .iter()
            .find(|(n, _)| n == "x-amz-security-token")
            .expect("token header");
        assert_eq!(token.1, "session-token-value");
        let authorization = headers
            .iter()
            .find(|(n, _)| n == "authorization")
            .expect("authorization");
        assert!(authorization.1.contains("x-amz-security-token"), "signed");
        assert!(authorization.1.contains("content-type"), "signed");
        assert!(
            authorization.1.contains("/us-east-1/bedrock/aws4_request"),
            "{}",
            authorization.1
        );
    }
}
