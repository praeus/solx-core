//! Shared request-sending helper used by every `Remote*Manager`.

use serde::de::DeserializeOwned;
use serde::Serialize;
use solx_surface::error::SolxError;

use crate::error::{read_response, transport_err};

/// POST `body` as JSON to `{base_url}{route}` with a bearer token, and
/// deserialize the response (success or error) via
/// [`crate::error::read_response`].
pub(crate) async fn post_json<Req, Resp>(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
    route: &str,
    body: &Req,
) -> Result<Resp, SolxError>
where
    Req: Serialize + ?Sized,
    Resp: DeserializeOwned,
{
    let resp = http
        .post(format!("{base_url}{route}"))
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(transport_err)?;
    read_response(resp).await
}
