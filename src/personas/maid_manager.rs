// Copyright 2015 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement, version 1.0.  This, along with the
// Licenses can be found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

use std::collections::HashMap;
use std::mem;

use error::{ClientError, InternalError};
use lru_time_cache::LruCache;
use maidsafe_utilities::serialisation;
use routing::{Authority, Data, MessageId, RequestContent, RequestMessage};
use sodiumoxide::crypto::hash::sha512;
use time::Duration;
use types::{Refresh, RefreshValue};
use utils;
use vault::RoutingNode;
use xor_name::XorName;

const DEFAULT_ACCOUNT_SIZE: u64 = 1_073_741_824;  // 1 GB
const DEFAULT_PAYMENT: u64 = 1_048_576;  // 1 MB

#[derive(RustcEncodable, RustcDecodable, PartialEq, Eq, Debug, Clone)]
pub struct Account {
    data_stored: u64,
    space_available: u64,
}

impl Default for Account {
    fn default() -> Account {
        Account {
            data_stored: 0,
            space_available: DEFAULT_ACCOUNT_SIZE,
        }
    }
}

impl Account {
    fn put_data(&mut self, size: u64) -> Result<(), ClientError> {
        if size > self.space_available {
            return Err(ClientError::LowBalance);
        }
        self.data_stored += size;
        self.space_available -= size;
        Ok(())
    }

    fn delete_data(&mut self, size: u64) {
        if self.data_stored < size {
            self.space_available += self.data_stored;
            self.data_stored = 0;
        } else {
            self.data_stored -= size;
            self.space_available += size;
        }
    }
}



pub struct MaidManager {
    accounts: HashMap<XorName, Account>,
    request_cache: LruCache<MessageId, RequestMessage>,
}

impl MaidManager {
    pub fn new() -> MaidManager {
        MaidManager {
            accounts: HashMap::new(),
            request_cache: LruCache::with_expiry_duration_and_capacity(Duration::minutes(5), 1000),
        }
    }

    pub fn handle_put(&mut self,
                      routing_node: &RoutingNode,
                      request: &RequestMessage)
                      -> Result<(), InternalError> {
        match request.content {
            RequestContent::Put(Data::Immutable(_), _) => {
                self.handle_put_immutable_data(routing_node, request)
            }
            RequestContent::Put(Data::Structured(_), _) => {
                self.handle_put_structured_data(routing_node, request)
            }
            _ => unreachable!("Error in vault demuxing"),
        }
    }

    pub fn handle_put_success(&mut self,
                              routing_node: &RoutingNode,
                              message_id: &MessageId)
                              -> Result<(), InternalError> {
        match self.request_cache.remove(message_id) {
            Some(client_request) => {
                // Send success response back to client
                let message_hash =
                    sha512::hash(&try!(serialisation::serialise(&client_request))[..]);
                let src = client_request.dst;
                let dst = client_request.src;
                let _ = routing_node.send_put_success(src, dst, message_hash, *message_id);
                Ok(())
            }
            None => Err(InternalError::FailedToFindCachedRequest(*message_id)),
        }
    }

    pub fn handle_put_failure(&mut self,
                              routing_node: &RoutingNode,
                              message_id: &MessageId,
                              external_error_indicator: &[u8])
                              -> Result<(), InternalError> {
        match self.request_cache.remove(message_id) {
            Some(client_request) => {
                // Refund account
                match self.accounts.get_mut(&utils::client_name(&client_request.src)) {
                    Some(account) => {
                        account.delete_data(DEFAULT_PAYMENT /* data.payload_size() as u64 */)
                    }
                    None => return Ok(()),
                }
                // Send failure response back to client
                let error =
                    try!(serialisation::deserialise::<ClientError>(external_error_indicator));
                self.reply_with_put_failure(routing_node, client_request, *message_id, &error)
            }
            None => Err(InternalError::FailedToFindCachedRequest(*message_id)),
        }
    }

    pub fn handle_refresh(&mut self, name: XorName, account: Account) {
        let _ = self.accounts.insert(name, account);
    }

    pub fn handle_churn(&mut self, routing_node: &RoutingNode) {
        // Only retain accounts for which we're still in the close group
        let accounts = mem::replace(&mut self.accounts, HashMap::new());
        self.accounts = accounts.into_iter()
                                .filter(|&(ref maid_name, ref account)| {
                                    match routing_node.close_group(*maid_name) {
                                        Ok(None) => {
                                            trace!("No longer a MM for {}", maid_name);
                                            false
                                        }
                                        Ok(Some(_)) => {
                                            self.send_refresh(routing_node, maid_name, account);
                                            true
                                        }
                                        Err(error) => {
                                            error!("Failed to get close group: {:?} for {}",
                                                   error,
                                                   maid_name);
                                            false
                                        }
                                    }
                                })
                                .collect();
    }

    fn send_refresh(&self, routing_node: &RoutingNode, maid_name: &XorName, account: &Account) {
        let src = Authority::ClientManager(*maid_name);
        let refresh = Refresh::new(maid_name, RefreshValue::MaidManagerAccount(account.clone()));
        if let Ok(serialised_refresh) = serialisation::serialise(&refresh) {
            trace!("MM sending refresh for account {}", src.name());
            let _ = routing_node.send_refresh_request(src, serialised_refresh);
        }
    }

    fn handle_put_immutable_data(&mut self,
                                 routing_node: &RoutingNode,
                                 request: &RequestMessage)
                                 -> Result<(), InternalError> {
        let (data, message_id) = if let RequestContent::Put(Data::Immutable(ref data),
                                                            ref message_id) = request.content {
            (Data::Immutable(data.clone()), message_id)
        } else {
            unreachable!("Logic error")
        };
        let client_name = utils::client_name(&request.src);
        self.forward_put_request(routing_node, client_name, data, *message_id, request)
    }

    fn handle_put_structured_data(&mut self,
                                  routing_node: &RoutingNode,
                                  request: &RequestMessage)
                                  -> Result<(), InternalError> {
        let (data, type_tag, message_id) = if let RequestContent::Put(Data::Structured(ref data),
                                                                      ref message_id) =
                                                  request.content {
            (Data::Structured(data.clone()),
             data.get_type_tag(),
             message_id)
        } else {
            unreachable!("Logic error")
        };

        // If the type_tag is 0, the account must not exist, else it must exist.
        let client_name = utils::client_name(&request.src);
        if type_tag == 0 {
            if self.accounts.contains_key(&client_name) {
                let error = ClientError::AccountExists;
                try!(self.reply_with_put_failure(routing_node,
                                                 request.clone(),
                                                 *message_id,
                                                 &error));
                return Err(InternalError::Client(error));
            }

            // Create the account, the SD incurs charge later on
            let _ = self.accounts.insert(client_name, Account::default());
        }
        self.forward_put_request(routing_node, client_name, data, *message_id, request)
    }

    fn forward_put_request(&mut self,
                           routing_node: &RoutingNode,
                           client_name: XorName,
                           data: Data,
                           message_id: MessageId,
                           request: &RequestMessage)
                           -> Result<(), InternalError> {
        // Account must already exist to Put Data.
        let result = self.accounts
                         .get_mut(&client_name)
                         .ok_or(ClientError::NoSuchAccount)
                         .and_then(|account| {
                             account.put_data(DEFAULT_PAYMENT /* data.payload_size() as u64 */)
                         });
        if let Err(error) = result {
            trace!("MM responds put_failure of data {}, due to error {:?}",
                   data.name(),
                   error);
            try!(self.reply_with_put_failure(routing_node, request.clone(), message_id, &error));
            return Err(InternalError::Client(error));
        }

        {
            // forwarding data_request to NAE Manager
            let src = request.dst.clone();
            let dst = Authority::NaeManager(data.name());
            trace!("MM forwarding put request to {:?}", dst);
            let _ = routing_node.send_put_request(src, dst, data, message_id);
        }

        if let Some(prior_request) = self.request_cache
                                         .insert(message_id, request.clone()) {
            error!("Overwrote existing cached request: {:?}", prior_request);
        }

        Ok(())
    }

    fn reply_with_put_failure(&self,
                              routing_node: &RoutingNode,
                              request: RequestMessage,
                              message_id: MessageId,
                              error: &ClientError)
                              -> Result<(), InternalError> {
        let src = request.dst.clone();
        let dst = request.src.clone();
        let external_error_indicator = try!(serialisation::serialise(error));
        let _ = routing_node.send_put_failure(src,
                                              dst,
                                              request,
                                              external_error_indicator,
                                              message_id);
        Ok(())
    }
}

impl Default for MaidManager {
    fn default() -> MaidManager {
        MaidManager::new()
    }
}



#[cfg(all(test, feature = "use-mock-routing"))]
mod test {
    use super::*;
    use error::{ClientError, InternalError};
    use maidsafe_utilities::serialisation;
    use rand::{thread_rng, random};
    use rand::distributions::{IndependentSample, Range};
    use routing::{Authority, Data, ImmutableData, ImmutableDataType, MessageId, RequestContent,
                  RequestMessage, ResponseContent, StructuredData};
    use sodiumoxide::crypto::hash::sha512;
    use sodiumoxide::crypto::sign;
    use std::sync::mpsc;
    use types::Refresh;
    use utils;
    use utils::generate_random_vec_u8;
    use vault::RoutingNode;
    use xor_name::XorName;

    #[test]
    fn account_ok() {
        let mut account = Account::default();

        assert_eq!(0, account.data_stored);
        assert_eq!(super::DEFAULT_ACCOUNT_SIZE, account.space_available);
        assert!(account.put_data(super::DEFAULT_ACCOUNT_SIZE).is_ok());
        assert_eq!(super::DEFAULT_ACCOUNT_SIZE, account.data_stored);
        assert_eq!(0, account.space_available);

        account.delete_data(super::DEFAULT_ACCOUNT_SIZE);

        assert_eq!(0, account.data_stored);
        assert_eq!(super::DEFAULT_ACCOUNT_SIZE, account.space_available);
    }

    #[test]
    fn account_err() {
        let mut account = Account::default();

        assert_eq!(0, account.data_stored);
        assert_eq!(super::DEFAULT_ACCOUNT_SIZE, account.space_available);
        assert!(account.put_data(super::DEFAULT_ACCOUNT_SIZE).is_ok());
        assert_eq!(super::DEFAULT_ACCOUNT_SIZE, account.data_stored);
        assert_eq!(0, account.space_available);
        assert!(account.put_data(1).is_err());
        assert_eq!(super::DEFAULT_ACCOUNT_SIZE, account.data_stored);
        assert_eq!(0, account.space_available);
    }


    struct Environment {
        our_authority: Authority,
        client: Authority,
        routing: RoutingNode,
        maid_manager: MaidManager,
    }

    fn environment_setup() -> Environment {
        let routing = unwrap_result!(RoutingNode::new(mpsc::channel().0));
        let from = random::<XorName>();
        let client;

        loop {
            let keys = sign::gen_keypair();
            let name = XorName(sha512::hash(&keys.0[..]).0);
            if let Ok(Some(_)) = routing.close_group(name) {
                client = Authority::Client {
                    client_key: keys.0,
                    peer_id: random(),
                    proxy_node_name: from,
                };
                break;
            }
        }

        Environment {
            our_authority: Authority::ClientManager(utils::client_name(&client)),
            client: client,
            routing: routing,
            maid_manager: MaidManager::new(),
        }
    }

    fn create_account(env: &mut Environment) {
        if let Authority::Client { client_key, .. } = env.client {
            let identifier = random::<XorName>();
            let sd = unwrap_result!(StructuredData::new(0, identifier, 0, vec![], vec![client_key], vec![], None));
            let message_id = MessageId::new();
            let request = RequestMessage {
                src: env.client.clone(),
                dst: env.our_authority.clone(),
                content: RequestContent::Put(Data::Structured(sd), message_id),
            };

            if let Ok(()) = env.maid_manager.handle_put(&env.routing, &request) {} else {
                unreachable!()
            }
        };
    }

    fn get_close_node(env: &Environment) -> XorName {
        let mut name = random::<XorName>();

        loop {
            if let Ok(Some(_)) = env.routing.close_group(name) {
                return name
            } else {
                name = random::<XorName>();
            }
        }
    }

    #[cfg_attr(feature="clippy", allow(indexing_slicing))]
    fn lose_close_node(env: &Environment) -> XorName {
        loop {
            if let Ok(Some(close_group)) = env.routing.close_group(*env.our_authority.name()) {
                let mut rng = thread_rng();
                let range = Range::new(0, close_group.len());
                let our_name = if let Ok(ref name) = env.routing.name() {
                    *name
                } else {
                    unreachable!()
                };
                loop {
                    let index = range.ind_sample(&mut rng);
                    if close_group[index] != our_name {
                        return close_group[index]
                    }
                }
            }
        }
    }


    #[test]
    #[cfg_attr(feature="clippy", allow(indexing_slicing))]
    fn handle_put_without_account() {
        let mut env = environment_setup();

        // Try with valid ImmutableData before account is created
        let immutable_data = ImmutableData::new(ImmutableDataType::Normal, generate_random_vec_u8(1024));
        let message_id = MessageId::new();
        let valid_request = RequestMessage {
            src: env.client.clone(),
            dst: env.our_authority.clone(),
            content: RequestContent::Put(Data::Immutable(immutable_data), message_id),
        };

        if let Err(InternalError::Client(ClientError::NoSuchAccount)) =
               env.maid_manager.handle_put(&env.routing, &valid_request) {} else {
            unreachable!()
        }

        let put_requests = env.routing.put_requests_given();

        assert!(put_requests.is_empty());

        let put_failures = env.routing.put_failures_given();

        assert_eq!(put_failures.len(), 1);
        assert_eq!(put_failures[0].src, env.our_authority);
        assert_eq!(put_failures[0].dst, env.client);

        if let ResponseContent::PutFailure{ ref id, ref request, ref external_error_indicator } =
               put_failures[0].content {
            assert_eq!(*id, message_id);
            assert_eq!(*request, valid_request);
            if let ClientError::NoSuchAccount =
                   unwrap_result!(serialisation::deserialise(external_error_indicator)) {} else {
                unreachable!()
            }
        } else {
            unreachable!()
        }
    }

    #[test]
    #[cfg_attr(feature="clippy", allow(indexing_slicing))]
    fn handle_put_with_account() {
        let mut env = environment_setup();
        create_account(&mut env);

        let immutable_data = ImmutableData::new(ImmutableDataType::Normal, generate_random_vec_u8(1024));
        let message_id = MessageId::new();
        let valid_request = RequestMessage {
            src: env.client.clone(),
            dst: env.our_authority.clone(),
            content: RequestContent::Put(Data::Immutable(immutable_data.clone()), message_id),
        };

        if let Ok(()) = env.maid_manager.handle_put(&env.routing, &valid_request) {} else {
            unreachable!()
        }

        let put_failures = env.routing.put_failures_given();
        assert!(put_failures.is_empty());

        let put_requests = env.routing.put_requests_given();

        // put_requests[0] - account creation.
        assert_eq!(put_requests.len(), 2);
        assert_eq!(put_requests[1].src, env.our_authority);
        assert_eq!(put_requests[1].dst, Authority::NaeManager(immutable_data.name()));

        if let RequestContent::Put(Data::Immutable(ref data), ref id) = put_requests[1].content {
            assert_eq!(*data, immutable_data);
            assert_eq!(*id, message_id);
        } else {
            unreachable!()
        }
    }

    #[test]
    #[cfg_attr(feature="clippy", allow(indexing_slicing, shadow_unrelated))]
    fn invalid_put_for_previously_created_account() {
        let mut env = environment_setup();
        create_account(&mut env);

        let immutable_data = ImmutableData::new(ImmutableDataType::Normal, generate_random_vec_u8(1024));
        let message_id = MessageId::new();
        let valid_request = RequestMessage {
            src: env.client.clone(),
            dst: env.our_authority.clone(),
            content: RequestContent::Put(Data::Immutable(immutable_data.clone()), message_id),
        };

        if let Ok(()) = env.maid_manager.handle_put(&env.routing, &valid_request) {} else {
            unreachable!()
        }

        let put_failures = env.routing.put_failures_given();
        assert!(put_failures.is_empty());

        let put_requests = env.routing.put_requests_given();

        assert_eq!(put_requests.len(), 2);
        assert_eq!(put_requests[1].src, env.our_authority);
        assert_eq!(put_requests[1].dst, Authority::NaeManager(immutable_data.name()));

        if let RequestContent::Put(Data::Immutable(ref data), ref id) = put_requests[1].content {
            assert_eq!(*data, immutable_data);
            assert_eq!(*id, message_id);
        } else {
            unreachable!()
        }

        let client_key = if let Authority::Client { client_key, .. } = env.client { client_key } else {
            unreachable!()
        };

        let identifier = random::<XorName>();
        let sd = unwrap_result!(StructuredData::new(0, identifier, 0, vec![], vec![client_key], vec![], None));
        let message_id = MessageId::new();
        let valid_request = RequestMessage {
            src: env.client.clone(),
            dst: env.our_authority.clone(),
            content: RequestContent::Put(Data::Structured(sd), message_id),
        };

        if let Err(InternalError::Client(ClientError::AccountExists)) =
               env.maid_manager.handle_put(&env.routing, &valid_request) {} else {
            unreachable!()
        }

        let put_failures = env.routing.put_failures_given();

        assert_eq!(put_failures.len(), 1);
        assert_eq!(put_failures[0].src, env.our_authority);
        assert_eq!(put_failures[0].dst, env.client);

        if let ResponseContent::PutFailure{ ref id, ref request, ref external_error_indicator } =
               put_failures[0].content {
            assert_eq!(*id, message_id);
            assert_eq!(*request, valid_request);
            if let ClientError::AccountExists =
                   unwrap_result!(serialisation::deserialise(external_error_indicator)) {} else {
                unreachable!()
            }
        } else {
            unreachable!()
        }
    }

    #[test]
    #[cfg_attr(feature="clippy", allow(indexing_slicing, shadow_unrelated))]
    fn handle_put_success() {
        let mut env = environment_setup();
        create_account(&mut env);

        let immutable_data = ImmutableData::new(ImmutableDataType::Normal, generate_random_vec_u8(1024));
        let message_id = MessageId::new();
        let valid_request = RequestMessage {
            src: env.client.clone(),
            dst: env.our_authority.clone(),
            content: RequestContent::Put(Data::Immutable(immutable_data.clone()), message_id),
        };

        if let Ok(()) = env.maid_manager.handle_put(&env.routing, &valid_request) {} else {
            unreachable!()
        }

        let put_failures = env.routing.put_failures_given();
        assert!(put_failures.is_empty());

        let put_requests = env.routing.put_requests_given();

        assert_eq!(put_requests.len(), 2);
        assert_eq!(put_requests[1].src, env.our_authority);
        assert_eq!(put_requests[1].dst, Authority::NaeManager(immutable_data.name()));

        if let RequestContent::Put(Data::Immutable(ref data), ref id) = put_requests[1].content {
            assert_eq!(*data, immutable_data);
            assert_eq!(*id, message_id);
        } else {
            unreachable!()
        }

        // Valid case.
        if let Ok(()) = env.maid_manager.handle_put_success(&env.routing, &message_id) {} else {
            unreachable!()
        }

        let put_successes = env.routing.put_successes_given();

        assert_eq!(put_successes.len(), 1);
        assert_eq!(put_successes[0].src, env.our_authority);
        assert_eq!(put_successes[0].dst, env.client);

        if let ResponseContent::PutSuccess(ref digest, ref id) = put_successes[0].content {
            if let Ok(serialised_request) = serialisation::serialise(&valid_request) {
                assert_eq!(*digest, sha512::hash(&serialised_request[..]));
            }
            assert_eq!(*id, message_id);
        } else {
            unreachable!()
        }

        // Invalid case.
        let message_id = MessageId::new();

        if let Err(InternalError::FailedToFindCachedRequest(id)) =
                env.maid_manager.handle_put_success(&env.routing, &message_id) {
            assert_eq!(message_id, id);
        } else {
            unreachable!()
        }
    }

    #[test]
    #[cfg_attr(feature="clippy", allow(indexing_slicing, shadow_unrelated))]
    fn handle_put_failure() {
        let mut env = environment_setup();
        create_account(&mut env);

        let client_key = if let Authority::Client { client_key, .. } = env.client { client_key } else {
            unreachable!()
        };
        let identifier = random::<XorName>();
        let sd = unwrap_result!(StructuredData::new(1, identifier, 0, vec![], vec![client_key], vec![], None));
        let message_id = MessageId::new();
        let valid_request = RequestMessage {
            src: env.client.clone(),
            dst: env.our_authority.clone(),
            content: RequestContent::Put(Data::Structured(sd.clone()), message_id),
        };

        if let Ok(()) = env.maid_manager.handle_put(&env.routing, &valid_request) {} else {
            unreachable!()
        }

        let put_failures = env.routing.put_failures_given();
        assert!(put_failures.is_empty());

        let put_requests = env.routing.put_requests_given();

        assert_eq!(put_requests.len(), 2);
        assert_eq!(put_requests[1].src, env.our_authority);
        assert_eq!(put_requests[1].dst, Authority::NaeManager(sd.name()));

        if let RequestContent::Put(Data::Structured(ref data), ref id) = put_requests[1].content {
            assert_eq!(*data, sd);
            assert_eq!(*id, message_id);
        } else {
            unreachable!()
        }

        // Valid case.
        let error = ClientError::NoSuchData;
        if let Ok(error_indicator) = serialisation::serialise(&error) {
            if let Ok(()) = env.maid_manager.handle_put_failure(&env.routing, &message_id, &error_indicator[..]) {}
            else {
                unreachable!()
            }
        } else {
            unreachable!()
        }

        let put_failures = env.routing.put_failures_given();

        assert_eq!(put_failures.len(), 1);
        assert_eq!(put_failures[0].src, env.our_authority);
        assert_eq!(put_failures[0].dst, env.client);

        if let ResponseContent::PutFailure{ ref id, ref request, ref external_error_indicator } =
               put_failures[0].content {
            assert_eq!(*id, message_id);
            assert_eq!(*request, valid_request);
            if let Ok(error_indicator) = serialisation::serialise(&error) {
                assert_eq!(*external_error_indicator, error_indicator);
            } else {
                unreachable!()
            }
        } else {
            unreachable!()
        }

        // Invalid case.
        let message_id = MessageId::new();
        if let Ok(error_indicator) = serialisation::serialise(&error) {
            if let Err(InternalError::FailedToFindCachedRequest(id)) =
                    env.maid_manager.handle_put_failure(&env.routing, &message_id, &error_indicator[..]) {
                assert_eq!(message_id, id);
            } else {
                unreachable!()
            }
        } else {
            unreachable!()
        }
    }

    #[test]
    #[cfg_attr(feature="clippy", allow(indexing_slicing, shadow_unrelated))]
    fn churn_refresh() {
        let mut env = environment_setup();
        create_account(&mut env);

        env.routing.node_added_event(get_close_node(&env));
        env.maid_manager.handle_churn(&env.routing);

        let mut refresh_count = 0;
        let refresh_requests = env.routing.refresh_requests_given();

        if let Ok(Some(_)) = env.routing.close_group(utils::client_name(&env.client)) {
            assert_eq!(refresh_requests.len(), 1);
            assert_eq!(refresh_requests[0].src, env.our_authority);
            assert_eq!(refresh_requests[0].dst, env.our_authority);

            if let RequestContent::Refresh(ref serialised_refresh) = refresh_requests[0].content {
               if let Ok(refresh) = serialisation::deserialise(&serialised_refresh) {
                    let refresh: Refresh = refresh;
                    assert_eq!(refresh.name, utils::client_name(&env.client));
                } else {
                    unreachable!()
                }
            } else {
                unreachable!()
            }
            refresh_count += 1;
        } else {
            assert_eq!(refresh_requests.len(), 0);
        }

        env.routing.node_lost_event(lose_close_node(&env));
        env.maid_manager.handle_churn(&env.routing);

        let refresh_requests = env.routing.refresh_requests_given();

        if let Ok(Some(_)) = env.routing.close_group(utils::client_name(&env.client)) {
            assert_eq!(refresh_requests.len(), refresh_count + 1);
            assert_eq!(refresh_requests[refresh_count].src, env.our_authority);
            assert_eq!(refresh_requests[refresh_count].dst, env.our_authority);

            if let RequestContent::Refresh(ref serialised_refresh) = refresh_requests[refresh_count].content {
               if let Ok(refresh) = serialisation::deserialise(&serialised_refresh) {
                    let refresh: Refresh = refresh;
                    assert_eq!(refresh.name, utils::client_name(&env.client));
                } else {
                    unreachable!()
                }
            } else {
                unreachable!()
            }
            // refresh_count += 1;
        } else {
            assert_eq!(refresh_requests.len(), refresh_count);
        }
    }
}
