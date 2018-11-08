use crypto::crypto_rand::CryptoRandom;

use super::MutableFunderHandler;
use super::super::types::{FriendMessage,
                            FriendInconsistencyError, ChannelerConfig,
                            FriendStatus, ResetTerms,
                            FunderOutgoingComm};

pub enum HandleInitError {
}

#[allow(unused)]
impl<A: Clone + 'static, R: CryptoRandom> MutableFunderHandler<A,R> {

    pub fn handle_init(&mut self) {
        let mut enabled_friends = Vec::new();
        for (friend_public_key, friend) in &self.state.friends {
            match friend.status {
                FriendStatus::Enable => {
                    enabled_friends.push((friend.remote_public_key.clone(),
                        friend.remote_address.clone()));
                },
                FriendStatus::Disable => continue,
            };
        }

        for enabled_friend in enabled_friends {
            // Notify Channeler:
            let channeler_config = ChannelerConfig::AddFriend(enabled_friend);
            self.add_outgoing_comm(FunderOutgoingComm::ChannelerConfig(channeler_config));
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    use std::rc::Rc;

    use crate::state::{FunderState, FunderMutation};
    use crate::ephemeral::FunderEphemeral;

    use futures::executor::ThreadPool;
    use futures::{future, FutureExt};
    use futures::task::SpawnExt;
    use identity::{create_identity, IdentityClient};

    use crypto::test_utils::DummyRandom;
    use crypto::identity::{SoftwareEd25519Identity,
                            generate_pkcs8_key_pair, PUBLIC_KEY_LEN,
                            PublicKey};


    async fn task_handle_init_basic(identity_client: IdentityClient) {

        let local_pk = await!(identity_client.request_public_key()).unwrap();
        let pk_b = PublicKey::from(&[0xbb; PUBLIC_KEY_LEN]);

        let mut state = FunderState::new(&local_pk);
        let f_mutation = FunderMutation::AddFriend((pk_b, 3u32)); // second arg is address
        state.mutate(&f_mutation);

        let ephemeral = FunderEphemeral::new(&state);
        let rng = DummyRandom::new(&[2u8]);

        let mutable_funder_handler = MutableFunderHandler {
            state,
            ephemeral,
            identity_client: identity_client.clone(),
            rng: Rc::new(rng),
            mutations: Vec::new(),
            outgoing_comms: Vec::new(),
            responses_received: Vec::new(),
        };
    }

    #[test]
    fn test_handle_init_basic() {
        // Start identity service:
        let mut thread_pool = ThreadPool::new().unwrap();

        let rng = DummyRandom::new(&[1u8]);
        let pkcs8 = generate_pkcs8_key_pair(&rng);
        let identity = SoftwareEd25519Identity::from_pkcs8(&pkcs8).unwrap();
        let (requests_sender, identity_server) = create_identity(identity);
        let identity_client = IdentityClient::new(requests_sender);
        thread_pool.spawn(identity_server.then(|_| future::ready(()))).unwrap();

        thread_pool.run(task_handle_init_basic(identity_client));
    }
}
