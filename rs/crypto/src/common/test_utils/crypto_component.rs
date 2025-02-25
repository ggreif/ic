use crate::CryptoComponentFatClient;
use ic_crypto_internal_csp::public_key_store::PublicKeyStore;
use ic_crypto_internal_csp::secret_key_store::SecretKeyStore;
use ic_crypto_internal_csp::{CryptoServiceProvider, Csp};
use ic_interfaces_registry::RegistryClient;
use ic_logger::replica_logger::no_op_logger;
use ic_types_test_utils::ids::node_test_id;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;
use std::sync::Arc;

const NODE_ID: u64 = 42;

/// Note that `S: 'static` is required so that `CspTlsHandshakeSignerProvider`
/// can be implemented for [Csp]. See the documentation of the respective `impl`
/// block for more details on the meaning of `S: 'static`.
pub fn crypto_component_with<S: SecretKeyStore + 'static, P: PublicKeyStore + 'static>(
    registry_client: Arc<dyn RegistryClient>,
    secret_key_store: S,
    public_key_store: P,
) -> CryptoComponentFatClient<impl CryptoServiceProvider> {
    let csprng = ChaCha20Rng::seed_from_u64(42);
    let csp = Csp::of(csprng, secret_key_store, public_key_store);

    // The node id is currently irrelevant for the tests, so we set it to a constant
    // for now.
    CryptoComponentFatClient::new_with_csp_and_fake_node_id(
        csp,
        no_op_logger(),
        registry_client,
        node_test_id(NODE_ID),
    )
}
