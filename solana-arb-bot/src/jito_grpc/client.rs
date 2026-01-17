//! Jito gRPC Client
//! Handles authentication and bundle submission via gRPC

use anyhow::{anyhow, Result};
use solana_sdk::{
    signature::{Keypair, Signature, Signer},
    transaction::VersionedTransaction,
};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tonic::transport::{Channel, Endpoint};
use tonic::{Request, Status};

use super::protos::{
    auth::{
        auth_service_client::AuthServiceClient, GenerateAuthChallengeRequest,
        GenerateAuthTokensRequest, RefreshAccessTokenRequest, Role,
    },
    bundle::Bundle,
    searcher::{
        searcher_service_client::SearcherServiceClient, GetTipAccountsRequest,
        SendBundleRequest,
    },
    proto_packet_from_versioned_tx,
};

const AUTHORIZATION_HEADER: &str = "authorization";
const BEARER: &str = "Bearer ";

/// Jito gRPC client for bundle submission
pub struct JitoGrpcClient {
    endpoint: String,
    auth_keypair: Option<Arc<Keypair>>,
    bearer_token: Arc<RwLock<String>>,
}

impl JitoGrpcClient {
    /// Create a new Jito gRPC client
    /// If auth_keypair is provided, it will be used for authenticated access
    pub fn new(endpoint: &str, auth_keypair: Option<Arc<Keypair>>) -> Self {
        Self {
            endpoint: endpoint.to_string(),
            auth_keypair,
            bearer_token: Arc::new(RwLock::new(String::new())),
        }
    }

    /// Create a gRPC channel with TLS
    async fn create_channel(&self) -> Result<Channel> {
        let mut endpoint = Endpoint::from_shared(self.endpoint.clone())
            .map_err(|e| anyhow!("Invalid endpoint: {}", e))?;
        
        if self.endpoint.starts_with("https") {
            endpoint = endpoint
                .tls_config(tonic::transport::ClientTlsConfig::new())
                .map_err(|e| anyhow!("TLS config error: {}", e))?;
        }
        
        endpoint
            .connect()
            .await
            .map_err(|e| anyhow!("Failed to connect: {}", e))
    }

    /// Authenticate with the Jito block engine
    async fn authenticate(&self) -> Result<()> {
        let keypair = self.auth_keypair.as_ref()
            .ok_or_else(|| anyhow!("No auth keypair provided"))?;

        let channel = self.create_channel().await?;
        let mut auth_client = AuthServiceClient::new(channel);

        // Generate challenge
        let challenge_resp = auth_client
            .generate_auth_challenge(GenerateAuthChallengeRequest {
                role: Role::Searcher as i32,
                pubkey: keypair.pubkey().as_ref().to_vec(),
            })
            .await
            .map_err(|e| anyhow!("Failed to get challenge: {}", e))?
            .into_inner();

        // Sign challenge
        let challenge = format!("{}-{}", keypair.pubkey(), challenge_resp.challenge);
        let signed_challenge = keypair.sign_message(challenge.as_bytes()).as_ref().to_vec();

        // Get tokens
        let tokens = auth_client
            .generate_auth_tokens(GenerateAuthTokensRequest {
                challenge,
                client_pubkey: keypair.pubkey().as_ref().to_vec(),
                signed_challenge,
            })
            .await
            .map_err(|e| anyhow!("Failed to get tokens: {}", e))?
            .into_inner();

        let access_token = tokens.access_token
            .ok_or_else(|| anyhow!("No access token in response"))?;

        // Store token
        *self.bearer_token.write().unwrap() = access_token.value;

        tracing::info!("✅ Authenticated with Jito block engine");
        Ok(())
    }

    /// Get a searcher client (with or without auth)
    async fn get_searcher_client(&self) -> Result<SearcherServiceClient<Channel>> {
        let channel = self.create_channel().await?;
        Ok(SearcherServiceClient::new(channel))
    }

    /// Add auth header to request if we have a token
    fn add_auth_header<T>(&self, mut request: Request<T>) -> Request<T> {
        let token = self.bearer_token.read().unwrap();
        if !token.is_empty() {
            request.metadata_mut().insert(
                AUTHORIZATION_HEADER,
                format!("{}{}", BEARER, token).parse().unwrap(),
            );
        }
        request
    }

    /// Get tip accounts from the block engine
    pub async fn get_tip_accounts(&self) -> Result<Vec<String>> {
        let mut client = self.get_searcher_client().await?;
        
        let request = self.add_auth_header(Request::new(GetTipAccountsRequest {}));
        
        let response = client
            .get_tip_accounts(request)
            .await
            .map_err(|e| anyhow!("Failed to get tip accounts: {}", e))?;

        Ok(response.into_inner().accounts)
    }

    /// Send a bundle via gRPC
    pub async fn send_bundle(&self, transactions: &[VersionedTransaction]) -> Result<String> {
        // Authenticate if we have a keypair and no token
        if self.auth_keypair.is_some() && self.bearer_token.read().unwrap().is_empty() {
            self.authenticate().await?;
        }

        let mut client = self.get_searcher_client().await?;

        // Convert transactions to packets
        let packets: Vec<_> = transactions
            .iter()
            .map(proto_packet_from_versioned_tx)
            .collect();

        let bundle = Bundle {
            header: None,
            packets,
        };

        let request = self.add_auth_header(Request::new(SendBundleRequest {
            bundle: Some(bundle),
        }));

        tracing::info!("📤 Sending bundle via gRPC to {}", self.endpoint);

        let response = client
            .send_bundle(request)
            .await
            .map_err(|e| anyhow!("Failed to send bundle: {}", e))?;

        let uuid = response.into_inner().uuid;
        tracing::info!("✅ Bundle submitted! UUID: {}", uuid);

        // Return first transaction signature
        let first_sig = transactions
            .first()
            .map(|tx| tx.signatures[0].to_string())
            .unwrap_or_else(|| uuid.clone());

        Ok(first_sig)
    }
}
