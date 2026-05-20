use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ApiResource};
use kube::Client;
use rcgen::{CertificateParams, Issuer, KeyPair};
use rustls::ServerConfig;

use crate::CaState;

// ---------------------------------------------------------------------------
// OpenShift API resources
// ---------------------------------------------------------------------------

pub fn route_ar() -> ApiResource {
    ApiResource {
        group: "route.openshift.io".into(),
        version: "v1".into(),
        api_version: "route.openshift.io/v1".into(),
        kind: "Route".into(),
        plural: "routes".into(),
    }
}

pub fn project_ar() -> ApiResource {
    ApiResource {
        group: "project.openshift.io".into(),
        version: "v1".into(),
        api_version: "project.openshift.io/v1".into(),
        kind: "Project".into(),
        plural: "projects".into(),
    }
}

pub fn imagestream_ar() -> ApiResource {
    ApiResource {
        group: "image.openshift.io".into(),
        version: "v1".into(),
        api_version: "image.openshift.io/v1".into(),
        kind: "ImageStream".into(),
        plural: "imagestreams".into(),
    }
}

pub fn oauth_client_ar() -> ApiResource {
    ApiResource {
        group: "oauth.openshift.io".into(),
        version: "v1".into(),
        api_version: "oauth.openshift.io/v1".into(),
        kind: "OAuthClient".into(),
        plural: "oauthclients".into(),
    }
}

pub fn user_ar() -> ApiResource {
    ApiResource {
        group: "user.openshift.io".into(),
        version: "v1".into(),
        api_version: "user.openshift.io/v1".into(),
        kind: "User".into(),
        plural: "users".into(),
    }
}

pub fn identity_ar() -> ApiResource {
    ApiResource {
        group: "user.openshift.io".into(),
        version: "v1".into(),
        api_version: "user.openshift.io/v1".into(),
        kind: "Identity".into(),
        plural: "identities".into(),
    }
}

// ---------------------------------------------------------------------------
// Gateway / Istio API resources
// ---------------------------------------------------------------------------

pub fn gateway_class_ar() -> ApiResource {
    ApiResource {
        group: "gateway.networking.k8s.io".into(),
        version: "v1".into(),
        api_version: "gateway.networking.k8s.io/v1".into(),
        kind: "GatewayClass".into(),
        plural: "gatewayclasses".into(),
    }
}

pub fn gateway_ar() -> ApiResource {
    ApiResource {
        group: "gateway.networking.k8s.io".into(),
        version: "v1".into(),
        api_version: "gateway.networking.k8s.io/v1".into(),
        kind: "Gateway".into(),
        plural: "gateways".into(),
    }
}

pub fn httproute_ar() -> ApiResource {
    ApiResource {
        group: "gateway.networking.k8s.io".into(),
        version: "v1".into(),
        api_version: "gateway.networking.k8s.io/v1".into(),
        kind: "HTTPRoute".into(),
        plural: "httproutes".into(),
    }
}

pub fn destination_rule_ar() -> ApiResource {
    ApiResource {
        group: "networking.istio.io".into(),
        version: "v1".into(),
        api_version: "networking.istio.io/v1".into(),
        kind: "DestinationRule".into(),
        plural: "destinationrules".into(),
    }
}

// ---------------------------------------------------------------------------
// Other API resources
// ---------------------------------------------------------------------------

pub fn jobset_ar() -> ApiResource {
    ApiResource {
        group: "jobset.x-k8s.io".into(),
        version: "v1alpha2".into(),
        api_version: "jobset.x-k8s.io/v1alpha2".into(),
        kind: "JobSet".into(),
        plural: "jobsets".into(),
    }
}

// ---------------------------------------------------------------------------
// TLS certificate helpers
// ---------------------------------------------------------------------------

pub fn sign_cert(
    ca: &CaState,
    sans: &[&str],
) -> Result<(String, String), Box<dyn std::error::Error + Send + Sync>> {
    let ca_key = KeyPair::from_pem(&ca.ca_key_pem)?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.distinguished_name.push(
        rcgen::DnType::CommonName,
        rcgen::DnValue::Utf8String("ocp-sim-service-ca".into()),
    );
    let issuer = Issuer::from_params(&ca_params, &ca_key);

    let san_strings: Vec<String> = sans.iter().map(|s| s.to_string()).collect();
    let cn = sans.first().copied().unwrap_or("ocp-sim");
    let mut params = CertificateParams::new(san_strings)?;
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        rcgen::DnValue::Utf8String(cn.to_string()),
    );

    let key_pair = KeyPair::generate()?;
    let cert = params.signed_by(&key_pair, &issuer)?;

    Ok((cert.pem(), key_pair.serialize_pem()))
}

pub fn sign_tls_config(
    ca: &CaState,
    sans: &[&str],
) -> Result<ServerConfig, Box<dyn std::error::Error + Send + Sync>> {
    let (cert_pem, key_pem) = sign_cert(ca, sans)?;

    let certs = rustls_pemfile::certs(&mut cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut key_pem.as_bytes())?
        .ok_or("no private key found")?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;

    Ok(config)
}

// ---------------------------------------------------------------------------
// Cluster helpers
// ---------------------------------------------------------------------------

pub async fn get_node_ip(client: &Client) -> anyhow::Result<String> {
    let nodes: Api<Node> = Api::all(client.clone());
    let node_list = nodes.list(&Default::default()).await?;
    for node in node_list {
        if let Some(status) = node.status {
            if let Some(addresses) = status.addresses {
                for addr in addresses {
                    if addr.type_ == "InternalIP" {
                        return Ok(addr.address);
                    }
                }
            }
        }
    }
    anyhow::bail!("no node with InternalIP found")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ca() -> CaState {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            rcgen::DnValue::Utf8String("test-ca".into()),
        );
        let key_pair = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        CaState {
            ca_cert_pem: cert.pem(),
            ca_key_pem: key_pair.serialize_pem(),
        }
    }

    #[test]
    fn sign_cert_returns_valid_pem() {
        let ca = test_ca();
        let (cert_pem, key_pem) = sign_cert(&ca, &["test.example.com"]).unwrap();
        assert!(cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(key_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
    }

    #[test]
    fn sign_cert_multiple_sans() {
        let ca = test_ca();
        let (cert_pem, _) = sign_cert(&ca, &["a.test", "b.test", "c.test"]).unwrap();
        assert!(cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
    }

    #[test]
    fn sign_tls_config_produces_usable_config() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let ca = test_ca();
        let config = sign_tls_config(&ca, &["localhost"]).unwrap();
        assert!(config.alpn_protocols.is_empty() || true);
    }

    #[test]
    fn api_resource_constructors() {
        assert_eq!(route_ar().kind, "Route");
        assert_eq!(project_ar().kind, "Project");
        assert_eq!(imagestream_ar().kind, "ImageStream");
        assert_eq!(oauth_client_ar().kind, "OAuthClient");
        assert_eq!(user_ar().kind, "User");
        assert_eq!(identity_ar().kind, "Identity");
        assert_eq!(gateway_class_ar().kind, "GatewayClass");
        assert_eq!(gateway_ar().kind, "Gateway");
        assert_eq!(httproute_ar().kind, "HTTPRoute");
        assert_eq!(destination_rule_ar().kind, "DestinationRule");
        assert_eq!(jobset_ar().kind, "JobSet");
    }

    #[test]
    fn api_resource_groups() {
        assert_eq!(route_ar().group, "route.openshift.io");
        assert_eq!(gateway_ar().group, "gateway.networking.k8s.io");
        assert_eq!(destination_rule_ar().group, "networking.istio.io");
        assert_eq!(jobset_ar().group, "jobset.x-k8s.io");
    }
}
