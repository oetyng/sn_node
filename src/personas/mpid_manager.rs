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

use sodiumoxide::crypto::sign::PublicKey;

use chunk_store::ChunkStore;
use default_chunk_store;
use error::{ClientError, InternalError};
use maidsafe_utilities::serialisation::{deserialise, serialise};
use mpid_messaging::{self, MAX_INBOX_SIZE, MAX_OUTBOX_SIZE, MpidMessageWrapper};
use routing::{Authority, Data, PlainData, RequestContent, RequestMessage};
use vault::RoutingNode;
use xor_name::XorName;

#[derive(RustcEncodable, RustcDecodable, PartialEq, Eq, Debug, Clone)]
struct MailBox {
    allowance: u64,
    used_space: u64,
    space_available: u64,
    // key: msg or header's name; value: sender's public key
    mail_box: HashMap<XorName, Option<PublicKey>>,
}

impl MailBox {
    fn new(allowance: u64) -> MailBox {
        MailBox {
            allowance: allowance,
            used_space: 0,
            space_available: allowance,
            mail_box: HashMap::new()
        }
    }


    fn put(&mut self, size: u64, entry: &XorName, public_key: &Option<PublicKey>) -> bool {
        if size > self.space_available {
            return false;
        }
        if self.mail_box.contains_key(entry) {
            return false;
        }
        match self.mail_box.insert(entry.clone(), public_key.clone()) {
            Some(_) => {
                self.used_space += size;
                self.space_available -= size;
                true
            }
            None => false,
        }
    }

    #[allow(dead_code)]
    fn remove(&mut self, size: u64, entry: &XorName) -> bool {
        if !self.mail_box.contains_key(entry) {
            return false;
        }
        self.used_space -= size;
        self.space_available += size;
        match self.mail_box.remove(entry) {
            Some(_) => {
                self.used_space -= size;
                self.space_available += size;
                true
            }
            None => false,
        }
    }

    #[allow(dead_code)]
    fn has(&mut self, entry: &XorName) -> bool {
        self.mail_box.contains_key(entry)
    }
}

#[derive(RustcEncodable, RustcDecodable, PartialEq, Eq, Debug, Clone)]
struct Account {
    // account owners' registerred client proxies
    clients: Vec<Authority>,
    inbox: MailBox,
    outbox: MailBox,
}

impl Default for Account {
    // FIXME: Account Creation process required
    //   To bypass the the process for a simple network, allowance is granted by default
    fn default() -> Account {
        Account {
            clients: Vec::new(),
            inbox: MailBox::new(MAX_INBOX_SIZE as u64),
            outbox: MailBox::new(MAX_OUTBOX_SIZE as u64),
        }
    }
}

impl Account {
    fn put_into_outbox(&mut self, size: u64, entry: &XorName,
                       public_key: &Option<PublicKey>) -> bool {
        self.outbox.put(size, entry, public_key)
    }

    fn put_into_inbox(&mut self, size: u64, entry: &XorName,
                      public_key: &Option<PublicKey>) -> bool {
        self.inbox.put(size, entry, public_key)
    }

    #[allow(dead_code)]
    fn remove_from_outbox(&mut self, size: u64, entry: &XorName) -> bool {
        self.outbox.remove(size, entry)
    }

    #[allow(dead_code)]
    fn remove_from_inbox(&mut self, size: u64, entry: &XorName) -> bool {
        self.inbox.remove(size, entry)
    }
}

pub struct MpidManager {
    accounts: HashMap<XorName, Account>,
    chunk_store_inbox: ChunkStore,
    chunk_store_outbox: ChunkStore,
}

impl MpidManager {
    pub fn new() -> MpidManager {
        MpidManager {
            accounts: HashMap::new(),
            chunk_store_inbox: default_chunk_store::new().unwrap(),
            chunk_store_outbox: default_chunk_store::new().unwrap(),
        }
    }

    // The name of the PlainData is expected to be the Hash of its content
    pub fn handle_put(&mut self, routing_node: &RoutingNode, request: &RequestMessage)
            -> Result<(), InternalError> {
        let (data, message_id) = match request.content {
            RequestContent::Put(Data::PlainData(ref data), ref message_id) => {
                (data.clone(), message_id.clone())
            }
            _ => unreachable!("Error in vault demuxing"),
        };
        let mpid_message_wrapper = unwrap_option!(deserialise_wrapper(data.value()),
                                                  "Failed to parse MpidMessageWrapper");
        match mpid_message_wrapper {
            MpidMessageWrapper::PutHeader(_mpid_header) => {
                if self.chunk_store_inbox.has_chunk(&data.name()) {
                    return Err(InternalError::Client(ClientError::DataExists));;
                }
                // TODO: how the sender's public key get retained?
                if self.accounts
                       .entry(request.dst.get_name().clone())
                       .or_insert(Account::default())
                       .put_into_inbox(data.payload_size() as u64, &data.name(), &None) {
                    let _ = self.chunk_store_inbox.put(&data.name(), data.value());
                }
            }
            MpidMessageWrapper::PutMessage(mpid_message) => {
                if self.chunk_store_outbox.has_chunk(&data.name()) {
                    return Err(InternalError::Client(ClientError::DataExists));
                }
                // TODO: how the sender's public key get retained?
                if self.accounts
                       .entry(mpid_message.header().sender_name().clone())
                       .or_insert(Account::default())
                       .put_into_outbox(data.payload_size() as u64, &data.name(), &None) {
                    match self.chunk_store_outbox.put(&data.name(), data.value()) {
                        Err(err) => {
                            error!("Failed to store the full message to disk: {:?}", err);
                            return Err(InternalError::ChunkStore(err));
                        }
                        _ => {}
                    }
                    // Send notification to receiver's MpidManager
                    let src = request.dst.clone();
                    let dst = Authority::ClientManager(mpid_message.recipient().clone());
                    let wrapper = MpidMessageWrapper::PutHeader(mpid_message.header().clone());

                    let serialised_wrapper = match serialise(&wrapper) {
                        Ok(encoded) => encoded,
                        Err(error) => {
                            error!("Failed to serialise PutHeader wrapper: {:?}", error);
                            return Err(InternalError::Serialisation(error));
                        }
                    };
                    let name = match mpid_messaging::mpid_header_name(mpid_message.header()) {
                        Some(name) => name,
                        None => {
                            error!("Failed to calculate name of the header");
                            return Err(InternalError::Client(ClientError::NoSuchAccount));
                        }
                    };
                    let notification = Data::PlainData(PlainData::new(name, serialised_wrapper));
                    let _ = routing_node.send_put_request(src, dst, notification, message_id.clone());
                }
            }
            _ => unreachable!("Error in vault demuxing"),
        }
        Ok(())
    }

}

fn deserialise_wrapper(serialised_wrapper: &[u8]) -> Option<MpidMessageWrapper> {
    match deserialise::<MpidMessageWrapper>(serialised_wrapper) {
        Ok(data) => Some(data),
        Err(_) => None
    }
}
