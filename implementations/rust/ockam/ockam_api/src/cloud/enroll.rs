use std::borrow::Cow;

use minicbor::{Decode, Encode};
use serde::{Deserialize, Serialize};

#[cfg(feature = "tag")]
use ockam_core::TypeTag;
use ockam_core::{self, async_trait};

#[derive(Encode, Decode, Serialize, Deserialize, Debug)]
#[cfg_attr(test, derive(PartialEq, Eq, Clone))]
#[cbor(transparent)]
#[serde(transparent)]
pub struct Token<'a>(#[n(0)] pub Cow<'a, str>);

impl<'a> Token<'a> {
    pub fn new(token: impl Into<Cow<'a, str>>) -> Self {
        Self(token.into())
    }
}

pub enum AuthenticateToken<'a> {
    Auth0(auth0::AuthenticateAuth0Token<'a>),
    EnrollmentToken(enrollment_token::AuthenticateEnrollmentToken<'a>),
}

mod node {
    use minicbor::Decoder;
    use tracing::trace;

    use ockam_core::api::{Id, Request, Response, Status};
    use ockam_core::{self, Result, Route};
    use ockam_node::api::request;
    use ockam_node::Context;

    use crate::auth::types::Attributes;
    use crate::cloud::enroll::auth0::AuthenticateAuth0Token;
    use crate::cloud::enroll::enrollment_token::{
        AuthenticateEnrollmentToken, EnrollmentToken, RequestEnrollmentToken,
    };
    use crate::cloud::CloudRequestWrapper;
    use crate::nodes::NodeManager;

    use super::*;

    const TARGET: &str = "ockam_api::cloud::enroll";

    impl NodeManager {
        /// Executes an enrollment process to generate a new set of access tokens using the auth0 flow.
        pub(crate) async fn enroll_auth0(
            &mut self,
            ctx: &mut Context,
            req: &Request<'_>,
            dec: &mut Decoder<'_>,
        ) -> Result<Vec<u8>> {
            let req_wrapper: CloudRequestWrapper<AuthenticateAuth0Token> = dec.decode()?;
            let cloud_route = req_wrapper.route()?;
            let req_body: AuthenticateAuth0Token = req_wrapper.req;
            let req_body = AuthenticateToken::Auth0(req_body);

            trace!(target: TARGET, "executing auth0 flow");
            self.authenticate_token(ctx, req.id(), cloud_route, req_body)
                .await
        }

        /// Generates a token that will be associated to the passed attributes.
        pub(crate) async fn generate_enrollment_token(
            &mut self,
            ctx: &mut Context,
            req: &Request<'_>,
            dec: &mut Decoder<'_>,
        ) -> Result<Vec<u8>> {
            let req_wrapper: CloudRequestWrapper<Attributes> = dec.decode()?;
            let cloud_route = req_wrapper.route()?;
            let req_body: Attributes = req_wrapper.req;
            let req_body = RequestEnrollmentToken::new(req_body);

            let label = "enrollment_token_generator";
            trace!(target: TARGET, "generating tokens");

            let sc = self.secure_channel(cloud_route).await?;
            let route = self.cloud_service_route(&sc.to_string(), "enrollment_token_authenticator");

            let req_builder = Request::post("v0/").body(req_body);
            let res =
                match request(ctx, label, "request_enrollment_token", route, req_builder).await {
                    Ok(r) => Ok(r),
                    Err(err) => {
                        error!(?err, "Failed to create project");
                        Ok(Response::builder(req.id(), Status::InternalServerError)
                            .body(err.to_string())
                            .to_vec()?)
                    }
                };
            self.delete_secure_channel(ctx, sc).await?;
            res
        }

        /// Authenticates a token generated by `generate_enrollment_token`.
        pub(crate) async fn authenticate_enrollment_token(
            &mut self,
            ctx: &mut Context,
            req: &Request<'_>,
            dec: &mut Decoder<'_>,
        ) -> Result<Vec<u8>> {
            let req_wrapper: CloudRequestWrapper<EnrollmentToken> = dec.decode()?;
            let cloud_route = req_wrapper.route()?;
            let req_body: EnrollmentToken = req_wrapper.req;
            let req_body =
                AuthenticateToken::EnrollmentToken(AuthenticateEnrollmentToken::new(req_body));

            trace!(target: TARGET, "authenticating token");
            self.authenticate_token(ctx, req.id(), cloud_route, req_body)
                .await
        }

        async fn authenticate_token(
            &self,
            ctx: &mut Context,
            req_id: Id,
            cloud_route: Route,
            body: AuthenticateToken<'_>,
        ) -> Result<Vec<u8>> {
            // TODO: add AuthenticateAuth0Token to schema.cddl and use it here
            let schema = None;
            let label;
            let sc = self.secure_channel(cloud_route).await?;
            let r = match body {
                AuthenticateToken::Auth0(body) => {
                    label = "auth0_authenticator";
                    let route = self.cloud_service_route(&sc.to_string(), label);
                    let req_builder = Request::post("v0/enroll").body(body);
                    request(ctx, label, schema, route, req_builder).await
                }
                AuthenticateToken::EnrollmentToken(body) => {
                    label = "enrollment_token_authenticator";
                    let route = self.cloud_service_route(&sc.to_string(), label);
                    let req_builder = Request::post("v0/enroll").body(body);
                    request(ctx, label, schema, route, req_builder).await
                }
            };
            let res = match r {
                Ok(r) => Ok(r),
                Err(err) => {
                    error!(?err, "Failed to authenticate token");
                    Ok(Response::builder(req_id, Status::InternalServerError)
                        .body(err.to_string())
                        .to_vec()?)
                }
            };
            self.delete_secure_channel(ctx, sc).await?;
            res
        }
    }
}

pub mod auth0 {
    use super::*;

    #[async_trait::async_trait]
    pub trait Auth0TokenProvider: Send + Sync + 'static {
        async fn token(&self) -> ockam_core::Result<Auth0Token<'_>>;
    }

    // Req/Res types

    #[derive(serde::Deserialize, Debug, PartialEq, Eq)]
    pub struct DeviceCode<'a> {
        pub device_code: Cow<'a, str>,
        pub user_code: Cow<'a, str>,
        pub verification_uri: Cow<'a, str>,
        pub verification_uri_complete: Cow<'a, str>,
        pub expires_in: usize,
        pub interval: usize,
    }

    #[derive(serde::Deserialize, Debug, PartialEq, Eq)]
    pub struct TokensError<'a> {
        pub error: Cow<'a, str>,
        pub error_description: Cow<'a, str>,
    }

    #[derive(serde::Deserialize, Debug)]
    #[cfg_attr(test, derive(PartialEq, Eq, Clone))]
    pub struct Auth0Token<'a> {
        pub token_type: TokenType,
        pub access_token: Token<'a>,
    }

    #[derive(Encode, Decode, Debug)]
    #[cfg_attr(test, derive(Clone))]
    #[rustfmt::skip]
    #[cbor(map)]
    pub struct AuthenticateAuth0Token<'a> {
        #[cfg(feature = "tag")]
        #[n(0)] pub tag: TypeTag<1058055>,
        #[n(1)] pub token_type: TokenType,
        #[n(2)] pub access_token: Token<'a>,
    }

    impl<'a> AuthenticateAuth0Token<'a> {
        pub fn new(token: Auth0Token<'a>) -> Self {
            Self {
                #[cfg(feature = "tag")]
                tag: TypeTag,
                token_type: token.token_type,
                access_token: token.access_token,
            }
        }
    }

    // Auxiliary types

    #[derive(serde::Deserialize, Encode, Decode, Debug)]
    #[cfg_attr(test, derive(PartialEq, Eq, Clone))]
    #[rustfmt::skip]
    #[cbor(index_only)]
    pub enum TokenType {
        #[n(0)] Bearer,
    }
}

pub mod enrollment_token {
    use serde::Serialize;

    use crate::auth::types::Attributes;

    use super::*;

    // Main req/res types

    #[derive(Encode, Debug)]
    #[cfg_attr(test, derive(Decode, Clone))]
    #[rustfmt::skip]
    #[cbor(map)]
    pub struct RequestEnrollmentToken<'a> {
        #[cfg(feature = "tag")]
        #[n(0)] pub tag: TypeTag<8560526>,
        #[b(1)] pub attributes: Attributes<'a>,
    }

    impl<'a> RequestEnrollmentToken<'a> {
        pub fn new(attributes: Attributes<'a>) -> Self {
            Self {
                #[cfg(feature = "tag")]
                tag: TypeTag,
                attributes,
            }
        }
    }

    #[derive(Encode, Decode, Serialize, Debug)]
    #[cfg_attr(test, derive(Clone))]
    #[rustfmt::skip]
    #[cbor(map)]
    pub struct EnrollmentToken<'a> {
        #[cfg(feature = "tag")]
        #[serde(skip_serializing)]
        #[n(0)] pub tag: TypeTag<8932763>,
        #[n(1)] pub token: Token<'a>,
    }

    impl<'a> EnrollmentToken<'a> {
        pub fn new(token: Token<'a>) -> Self {
            Self {
                #[cfg(feature = "tag")]
                tag: TypeTag,
                token,
            }
        }
    }

    #[derive(Encode, Debug)]
    #[cfg_attr(test, derive(Decode, Clone))]
    #[rustfmt::skip]
    #[cbor(map)]
    pub struct AuthenticateEnrollmentToken<'a> {
        #[cfg(feature = "tag")]
        #[n(0)] pub tag: TypeTag<9463780>,
        #[n(1)] pub token: Token<'a>,
    }

    impl<'a> AuthenticateEnrollmentToken<'a> {
        pub fn new(token: EnrollmentToken<'a>) -> Self {
            Self {
                #[cfg(feature = "tag")]
                tag: TypeTag,
                token: token.token,
            }
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
pub(crate) mod tests {
    use minicbor::Decoder;
    use quickcheck::{Arbitrary, Gen};

    use ockam_core::api::{Method, Request, Response};
    use ockam_core::{Routed, Worker};
    use ockam_node::Context;

    use crate::cloud::enroll::auth0::AuthenticateAuth0Token;
    use crate::cloud::enroll::enrollment_token::{
        AuthenticateEnrollmentToken, EnrollmentToken, RequestEnrollmentToken,
    };
    use crate::cloud::enroll::Token;

    use super::*;

    pub(crate) mod auth0 {
        use crate::cloud::enroll::auth0::*;

        use super::*;

        pub struct MockAuth0Service;

        #[async_trait::async_trait]
        impl Auth0TokenProvider for MockAuth0Service {
            async fn token(&self) -> ockam_core::Result<Auth0Token<'_>> {
                Ok(Auth0Token {
                    token_type: TokenType::Bearer,
                    access_token: Token::new("access_token"),
                })
            }
        }

        #[derive(Debug, Clone)]
        struct RandomAuthorizedAuth0Token(AuthenticateAuth0Token<'static>);

        impl Arbitrary for RandomAuthorizedAuth0Token {
            fn arbitrary(g: &mut Gen) -> Self {
                RandomAuthorizedAuth0Token(AuthenticateAuth0Token::new(Auth0Token {
                    token_type: TokenType::Bearer,
                    access_token: Token::arbitrary(g),
                }))
            }
        }
    }

    mod enrollment_token {
        use super::*;

        #[derive(Debug, Clone)]
        struct RandomAuthorizedEnrollmentToken(AuthenticateEnrollmentToken<'static>);

        impl Arbitrary for RandomAuthorizedEnrollmentToken {
            fn arbitrary(g: &mut Gen) -> Self {
                RandomAuthorizedEnrollmentToken(AuthenticateEnrollmentToken::new(
                    EnrollmentToken::new(Token::arbitrary(g)),
                ))
            }
        }
    }

    impl Arbitrary for Token<'static> {
        fn arbitrary(g: &mut Gen) -> Self {
            Token(String::arbitrary(g).into())
        }
    }

    pub struct EnrollHandler;

    #[ockam_core::worker]
    impl Worker for EnrollHandler {
        type Message = Vec<u8>;
        type Context = Context;

        async fn handle_message(
            &mut self,
            ctx: &mut Context,
            msg: Routed<Self::Message>,
        ) -> ockam_core::Result<()> {
            let mut buf = Vec::new();
            {
                let mut dec = Decoder::new(msg.as_body());
                let req: Request = dec.decode()?;
                match (req.method(), req.path(), req.has_body()) {
                    (Some(Method::Post), "v0/", true) => {
                        if dec.decode::<RequestEnrollmentToken>().is_ok() {
                            Response::ok(req.id())
                                .body(EnrollmentToken::new(Token("ok".into())))
                                .encode(&mut buf)?;
                        } else {
                            dbg!();
                            Response::bad_request(req.id()).encode(&mut buf)?;
                        }
                    }
                    (Some(Method::Post), "v0/enroll", true) => {
                        if dec.clone().decode::<AuthenticateAuth0Token>().is_ok()
                            || dec.decode::<AuthenticateEnrollmentToken>().is_ok()
                        {
                            Response::ok(req.id()).encode(&mut buf)?;
                        } else {
                            dbg!();
                            Response::bad_request(req.id()).encode(&mut buf)?;
                        }
                    }
                    _ => {
                        dbg!();
                        Response::bad_request(req.id()).encode(&mut buf)?;
                    }
                }
            }
            ctx.send(msg.return_route(), buf).await
        }
    }
}
