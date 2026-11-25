use super::*;

pub(super) const SECRET: [u8; 32] = [0x5A_u8; 32];

pub(super) fn operator_ingress() -> LocalControlIngress {
    LocalControlIngress::new(
        "operator-control",
        "operator:local",
        AuthorizationMethod::SignedLocalApi,
        BTreeSet::from([Capability::PerformerStop, Capability::PerformerMute]),
        ControlSecret::new(SECRET),
    )
    .expect("operator ingress")
}

pub(super) fn request(action: &str, secret: &[u8]) -> super::OperatorRequest {
    super::OperatorRequest {
        action: action.to_owned(),
        secret: String::from_utf8(secret.to_vec()).expect("utf8 secret"),
        request_id: Some("req-test".to_owned()),
        argument: None,
    }
}

pub(super) fn dispatched(request: super::OperatorRequest) -> super::DispatchedOperatorRequest {
    let (tx, _rx) = tokio::sync::oneshot::channel();
    super::DispatchedOperatorRequest {
        request,
        respond: tx,
    }
}
