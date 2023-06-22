extern crate redis;

use std::{mem::MaybeUninit, time, str::FromStr};

use ethers::types::Address;
use redis::{Commands, Connection, RedisError};
use serde::Serialize;
use url::Url;

use crate::{votes::{Vote, VoteOption}, vote_registration::VoterRegistration, storage::{Network, fetch_storage_amount}, STARTING_AUTHORIZED_VOTER};

pub struct Redis {
    con: Connection,
}

#[derive(Debug, PartialEq)]
pub enum VoteStatus {
    DoesNotExist,
    InProgress(u64),
    Concluded,
    Started,
}

enum LookupKey {
    /// FIP number to vector of all votes
    Votes(u32, Network),
    /// VoteChoice and FIP number to total storage amount
    Storage(VoteOption, Network, u32),
    /// FIP number to timestamp of vote start
    Timestamp(u32, Network),
    /// Network and voter address to voter registration
    Voter(Network, Address),
    /// The network the address belongs to
    Network(Address),
    VoteStarters(Network),
}

impl LookupKey {
    fn to_bytes(&self) -> Vec<u8> {
        let (lookup_type, fip) = match self {
            // The first bit will be 0 or 1
            LookupKey::Votes(fip, ntw) => {
                (*ntw as u8, fip)
            },
            // The first bit will range between 2 and 8
            LookupKey::Storage(choice, ntw, fip) => {
                let choice = match choice {
                    VoteOption::Yay => 2,
                    VoteOption::Nay => 3,
                    VoteOption::Abstain => 4,
                };
                let nt = *ntw as u8 + 1; // 1 or 2
                (choice * nt, fip)
            }
            // The first bit will be 9 or 10
            LookupKey::Timestamp(fip, ntw) => (9 + *ntw as u8, fip),
            LookupKey::Voter(ntw, voter) => {
                let ntw = match ntw {
                    Network::Mainnet => 0,
                    Network::Testnet => 1,
                };
                let voter = voter.as_bytes();
                let mut bytes = Vec::with_capacity(21);
                bytes.push(ntw);
                bytes.extend_from_slice(voter);
                return bytes;
            },
            LookupKey::Network(voter) => {
                let voter = voter.as_bytes();
                let mut bytes = Vec::with_capacity(21);
                bytes.push(2);
                bytes.extend_from_slice(voter);
                return bytes;
            }
            LookupKey::VoteStarters(ntw) => {
                let bytes = vec![8,0,0,8,1,3,5, *ntw as u8];
                return bytes;
            }
        };
        let slice = unsafe {
            let mut key = MaybeUninit::<[u8; 5]>::uninit();
            let start = key.as_mut_ptr() as *mut u8;
            (start.add(0) as *mut [u8; 4]).write(fip.to_be_bytes());

            // This is the bit we set to 0 if we only want the token object
            (start.add(4) as *mut [u8; 1]).write([lookup_type as u8]);

            key.assume_init()
        };
        Vec::from(slice)
    }
}

#[derive(Serialize)]
struct VoteResults {
    yay: u64,
    nay: u64,
    abstain: u64,
    yay_storage_size: u128,
    nay_storage_size: u128,
    abstain_storage_size: u128,
}

impl Redis {
    pub fn new(path: impl Into<Url>) -> Result<Redis, RedisError> {
        let client = redis::Client::open(path.into())?;
        let con = client.get_connection()?;

        Ok(Self { con })
    }

    /*~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~/
    /                                 INITIALIZATION                                 /
    /~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~*/

    /// Creates a new vote in the database
    /// 
    /// * Voter must be registered before hand
    /// * Creates a new vector of voters from FIP number
    /// * Sets the timestamp of the vote start as now
    /// * For every storage provider the voter is authorized for, add their power to the vote choice
    async fn new_vote(
        &mut self,
        fip_number: impl Into<u32>,
        vote: Vote,
        voter: Address,
        ntw: Network,
    ) -> Result<(), RedisError> {
        let num = fip_number.into();

        if !self.is_authorized_starter(voter, ntw)? || voter != Address::from_str(STARTING_AUTHORIZED_VOTER).unwrap() {
            return Err(RedisError::from((
                redis::ErrorKind::TypeError,
                "Voter is not authorized to start a vote",
            )));
        }

        // Fetch the storage provider Id's that the voter is authorized for
        let authorized = self.voter_delegates(voter, ntw)?;

        if authorized.is_empty() {
            return Err(RedisError::from((
                redis::ErrorKind::TypeError,
                "Voter is not authorized for any storage providers",
            )));
        }

        let vote_key = LookupKey::Votes(num, ntw).to_bytes();
        let time_key = LookupKey::Timestamp(num, ntw).to_bytes();

        let choice = vote.choice();

        // Set a map of FIP number to vector of all votes
        self.con.set::<Vec<u8>, Vec<Vote>, ()>(vote_key, vec![vote])?;

        // Set a map of FIP to timestamp of vote start
        let timestamp = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.con.set::<Vec<u8>, u64, ()>(time_key, timestamp)?;

        // Add the storage providers power to their vote choice for the respective FIP
        for sp_id in authorized {
            self.add_storage(sp_id, ntw, choice.clone(), num).await?;
        }

        Ok(())
    }

    /// Registers a voter in the database
    /// 
    /// * Creates a lookup from voters address to their respective network
    /// * Creates a lookup from voters address to their authorized storage providers
    pub fn register_voter(&mut self, voter: VoterRegistration) -> Result<(), RedisError> {
        let key = LookupKey::Voter(voter.ntw(), voter.address()).to_bytes();

        self.set_network(voter.ntw(), voter.address())?;

        let authorized = voter.sp_ids();

        self.con.set::<Vec<u8>, Vec<u32>, ()>(key, authorized)?;

        Ok(())
    }

    pub fn unregister_voter(&mut self, voter: VoterRegistration) -> Result<(), RedisError> {
        let key = LookupKey::Voter(voter.ntw(), voter.address()).to_bytes();

        // Remove the voter from the network lookup
        self.remove_network(voter.address())?;

        self.con.del::<Vec<u8>, ()>(key)?;

        Ok(())
    }

    pub fn register_voter_starter(&mut self, voters: Vec<Address>, ntw: Network) -> Result<(), RedisError> {
        let key = LookupKey::VoteStarters(ntw).to_bytes();


        let mut current_voters = self.voter_starters(ntw)?;

        current_voters.extend(voters);

        let new_bytes = current_voters.into_iter().flat_map(|v| v.as_fixed_bytes().to_vec()).collect::<Vec<u8>>();

        self.con.set::<Vec<u8>, Vec<u8>, ()>(key, new_bytes)?;

        Ok(())
    }

    /*~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~/
    /                                     GETTERS                                    /
    /~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~*/

    pub fn is_authorized_starter(&mut self, voter: Address, ntw: Network) -> Result<bool, RedisError> {
        let voters = self.voter_starters(ntw)?;

        Ok(voters.contains(&voter))
    }

    /// Returns a json blob of the vote results for the FIP number
    /// 
    pub fn vote_results(&mut self, fip_number: impl Into<u32>, ntw: Network) -> Result<String, RedisError> {
        let mut yay = 0;
        let mut nay = 0;
        let mut abstain = 0;

        let num = fip_number.into();

        let votes = self.votes(num, ntw)?;

        for vote in votes {
            match vote.choice() {
                VoteOption::Yay => yay += 1,
                VoteOption::Nay => nay += 1,
                VoteOption::Abstain => abstain += 1,
            }
        }

        let results = VoteResults {
            yay,
            nay,
            abstain,
            yay_storage_size: self.get_storage(num, VoteOption::Yay, ntw)?,
            nay_storage_size: self.get_storage(num, VoteOption::Nay, ntw)?,
            abstain_storage_size: self.get_storage(num, VoteOption::Abstain, ntw)?,
        };

        match serde_json::to_string(&results) {
            Ok(j) => Ok(j),
            Err(_) => Err(RedisError::from((
                redis::ErrorKind::TypeError,
                "Error serializing vote results",
            ))),
        }
    }

    pub fn vote_status(&mut self, fip_number: impl Into<u32>, vote_length: impl Into<u64>, ntw: Network) -> Result<VoteStatus, RedisError> {
        let num = fip_number.into();
        let vote_key = LookupKey::Votes(num, ntw).to_bytes();
        let time_key = LookupKey::Timestamp(num, ntw).to_bytes();

        // Check if the FIP number exists in the database
        if !self.con.exists(vote_key)? {
            return Ok(VoteStatus::DoesNotExist);
        }

        // Check if the FIP number has a timestamp
        if !self.con.exists(time_key.clone())? {
            return Ok(VoteStatus::DoesNotExist);
        }

        // Check if the vote is still open
        let time_start: u64 = self.vote_start(num, ntw)?;
        let now = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let vote_length = vote_length.into();

        if now - time_start < vote_length {
            return Ok(VoteStatus::InProgress(time_start + vote_length - now));
        } else {
            return Ok(VoteStatus::Concluded);
        }
    }

    pub fn voter_delegates(&mut self, voter: Address, ntw: Network) -> Result<Vec<u32>, RedisError> {
        let key = LookupKey::Voter(ntw, voter).to_bytes();
        let delegates: Vec<u32> = self.con.get::<Vec<u8>, Vec<u32>>(key)?;
        Ok(delegates)
    }

    pub fn voter_starters(&mut self, ntw: Network) -> Result<Vec<Address>, RedisError> {
        let key = LookupKey::VoteStarters(ntw).to_bytes();

        let bytes: Vec<u8> = self.con.get::<Vec<u8>, Vec<u8>>(key)?;

        if bytes.len() % 20 != 0 {
            return Err(RedisError::from((
                redis::ErrorKind::TypeError,
                "Error retrieving vote starters, invalid length",
            )));
        }
        let addr_length = bytes.len() / 20;

        let mut starters: Vec<Address> = Vec::with_capacity(addr_length);
        for i in 0..addr_length {
            let start = i * 20;
            let end = start + 20;
            let addr = Address::from_slice( &bytes[start..end]);
            starters.push(addr);
        }

        Ok(starters)
    }

    fn get_storage(&mut self, fip_number: u32, vote: VoteOption, ntw: Network) -> Result<u128, RedisError> {
        let key = LookupKey::Storage(vote, ntw, fip_number).to_bytes();
        let storage_bytes: Vec<u8> = self.con.get::<Vec<u8>, Vec<u8>>(key)?;
        if storage_bytes.is_empty() {
            return Ok(0);
        }
        if storage_bytes.len() != 16 {
            return Err(RedisError::from((
                redis::ErrorKind::TypeError,
                "Error retrieving storage size",
            )));
        }
        let storage = u128::from_be_bytes(storage_bytes.try_into().unwrap());
        Ok(storage)
    }

    fn vote_start(&mut self, fip_number: impl Into<u32>, ntw: Network) -> Result<u64, RedisError> {
        let key = LookupKey::Timestamp(fip_number.into(), ntw).to_bytes();
        let timestamp: u64 = self.con.get::<Vec<u8>, u64>(key)?;
        Ok(timestamp)
    }

    fn votes(&mut self, fip_number: impl Into<u32>, ntw: Network) -> Result<Vec<Vote>, RedisError> {
        let key = LookupKey::Votes(fip_number.into(), ntw).to_bytes();
        let votes: Vec<Vote> = self.con.get::<Vec<u8>, Vec<Vote>>(key)?;
        Ok(votes)
    }

    pub fn network(&mut self, voter: Address) -> Result<Network, RedisError> {
        let key = LookupKey::Network(voter).to_bytes();
        let ntw: Network = self.con.get::<Vec<u8>, Network>(key)?;
        Ok(ntw)
    }

    /*~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~/
    /                                     SETTERS                                    /
    /~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~*/

    pub async fn add_vote<T>(&mut self, fip_number: T, vote: Vote, voter: Address) -> Result<(), RedisError>
    where
        T: Into<u32>,
    {
        let num: u32 = fip_number.into();

        let ntw = self.network(voter)?;

        // Fetch the storage provider Id's that the voter is authorized for
        let authorized = self.voter_delegates(voter, ntw)?;

        // If the voter is not authorized for any storage providers, throw an error
        if authorized.is_empty() {
            return Err(RedisError::from((
                redis::ErrorKind::TypeError,
                "Voter is not authorized for any storage providers",
            )));
        }

        // Check if a vote has been started for this FIP number
        let mut votes = self.votes(num, ntw)?;

        // If no votes exist, create a new vote
        if votes.is_empty() {
            self.new_vote(num, vote, voter, ntw).await?;
            return Ok(());
        }

        let key = LookupKey::Votes(num.into(), ntw).to_bytes();

        // If this vote is a duplicate throw an error
        if votes.contains(&vote) {
            return Err(RedisError::from((
                redis::ErrorKind::TypeError,
                "Vote already exists",
            )));
        }

        // Add the storage providers power to their vote choice for the respective FIP
        for sp_id in authorized {
            self.add_storage(sp_id, ntw, vote.choice(), num).await?;
        }

        if ntw == Network::Mainnet {
            // Add the vote to the list of votes
            votes.push(vote);
            self.con.set::<Vec<u8>, Vec<Vote>, ()>(key.clone(), votes)?;
        }

        Ok(())
    }

    pub fn flush_vote(&mut self, fip_number: impl Into<u32>, ntw: Network) -> Result<(), RedisError> {
        let key = LookupKey::Votes(fip_number.into(), ntw).to_bytes();
        self.con.del::<Vec<u8>, ()>(key)?;
        Ok(())
    }

    pub fn flush_all_votes(&mut self) -> Result<(), RedisError> {
        let keys: Vec<Vec<u8>> = self.con.keys("*")?;
        for key in keys {
            self.con.del::<Vec<u8>, ()>(key)?;
        }
        Ok(())
    }

    async fn add_storage(&mut self, sp_id: u32, ntw: Network, vote: VoteOption, fip_number: u32) -> Result<(), RedisError> {
        let key = LookupKey::Storage(vote.clone(), ntw, fip_number).to_bytes();

        let current_storage = self.get_storage(fip_number, vote, ntw)?;

        let new_storage = match fetch_storage_amount(sp_id, ntw).await {
            Ok(s) => s,
            Err(_) => return Err(RedisError::from((
                redis::ErrorKind::TypeError,
                "Error fetching storage amount",
            ))),
        };
        let storage = current_storage + new_storage;
        let storage_bytes = storage.to_be_bytes().to_vec();
        self.con.set::<Vec<u8>, Vec<u8>, ()>(key.clone(), storage_bytes)?;
        Ok(())
    }

    /// Creates a lookup from the voter to the network they are voting on
    fn set_network(&mut self, ntw: Network, voter: Address) -> Result<(), RedisError> {
        let key: Vec<u8> = LookupKey::Network(voter).to_bytes();
        self.con.set::<Vec<u8>, Network, ()>(key, ntw)?;
        Ok(())
    }

    /// Removes the lookup from the voter to the network they are voting on
    fn remove_network(&mut self, voter: Address) -> Result<(), RedisError> {
        let key: Vec<u8> = LookupKey::Network(voter).to_bytes();
        self.con.del::<Vec<u8>, ()>(key)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    use crate::votes::test_votes::*;
    use crate::vote_registration::test_voter_registration::*;

    async fn redis() -> Redis {
        let url = Url::parse("redis://127.0.0.1:6379").unwrap();
        let mut redis = Redis::new(url).unwrap();

        for i in 1..=10 {
            redis.flush_vote(i as u32, Network::Testnet).unwrap();
        }

        let vote_reg = test_reg().recover_vote_registration().await.unwrap();

        redis.register_voter(vote_reg).unwrap();

        redis
    }

    fn voter() -> Address {
        Address::from_str("0xf2361d2a9a0677e8ffd1515d65cf5190ea20eb56").unwrap()
    }

    #[tokio::test]
    async fn redis_votes() {
        let mut redis = redis().await;

        let res = redis.votes(5u32, Network::Testnet);

        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn redis_get_storage() {
        let mut redis = redis().await;

        let res = redis.get_storage(5u32, VoteOption::Yay, Network::Testnet);

        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn redis_add_storage() {
        let mut redis = redis().await;

        let res = redis.add_storage(6024u32, Network::Testnet, VoteOption::Yay, 5u32).await;

        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn redis_vote_start() {
        let mut redis = redis().await;

        let vote = test_vote(VoteOption::Yay, 4u32).vote().unwrap();
        let res = redis.add_vote(4u32, vote, voter()).await;
        assert!(res.is_ok());

        let res = redis.vote_start(4u32, Network::Testnet);

        match res {
            Ok(_) => {},
            Err(e) => panic!("Error: {}", e),
        }
    }

    #[tokio::test]
    async fn redis_vote_status() {
        let mut redis = redis().await;

        let vote = test_vote(VoteOption::Yay, 3u32).vote().unwrap();
        assert!(redis.add_vote(3u32, vote, voter()).await.is_ok());


        let vote_start = redis.vote_start(3u32, Network::Testnet).unwrap();

        tokio::time::sleep(time::Duration::from_secs(2)).await;

        let time_now = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let ongoing = time_now - vote_start + 1;
        let concluded = time_now - vote_start - 1;

        let res = redis.vote_status(3u32, ongoing, Network::Testnet);

        match res {
            Ok(_) => {},
            Err(e) => panic!("Error: {}", e),
        }
        assert_eq!(res.unwrap(), VoteStatus::InProgress(1));

        let res = redis.vote_status(3u32, concluded, Network::Testnet);

        match res {
            Ok(_) => {},
            Err(e) => panic!("Error: {}", e),
        }
        assert_eq!(res.unwrap(), VoteStatus::Concluded);

        let res = redis.vote_status(1234089398u32, concluded, Network::Testnet);

        match res {
            Ok(_) => {},
            Err(e) => panic!("Error: {}", e),
        }
        assert_eq!(res.unwrap(), VoteStatus::DoesNotExist);
    }

    #[tokio::test]
    async fn redis_add_vote() {
        let mut redis = redis().await;

        let vote = test_vote(VoteOption::Yay, 2u32).vote().unwrap();

        let res = redis.add_vote(2u32, vote, voter()).await;

        match res {
            Ok(_) => {},
            Err(e) => panic!("Error: {}", e),
        }
    }

    #[tokio::test]
    async fn redis_vote_results() {
        let mut redis = redis().await;
        let vote = test_vote(VoteOption::Yay, 1u32).vote().unwrap();

        let res = redis.add_vote(1u32, vote, voter()).await;
        println!("{:?}", res);
        assert!(res.is_ok());

        let res = redis.vote_results(1u32, Network::Testnet);

        match res {
            Ok(_) => {},
            Err(e) => panic!("Error: {}", e),
        }
    }

    #[tokio::test]
    async fn redis_flush_database() {
        let mut redis = redis().await;
        redis.flush_all_votes().unwrap();
    }
}