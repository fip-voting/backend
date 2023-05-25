extern crate redis;

use std::{mem::MaybeUninit, time};

use redis::{Commands, Connection, RedisError};
use serde::Serialize;
use url::Url;

use crate::votes::{Vote, VoteOption};

pub struct Redis {
    con: Connection,
}

pub enum VoteStatus {
    DoesNotExist,
    Open,
    Closed,
}

enum LookupKey {
    FipNumber(u32),
    Timestamp(u32),
}

impl LookupKey {
    fn to_bytes(&self) -> Vec<u8> {
        let (lookup_type, fip) = match self {
            LookupKey::FipNumber(fip) => (0, fip),
            LookupKey::Timestamp(fip) => (1, fip),
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

/*
   TODO: Set up table for tracking storage size of votes
*/

impl Redis {
    pub fn new(path: impl Into<Url>) -> Result<Redis, RedisError> {
        let client = redis::Client::open(path.into())?;
        let con = client.get_connection()?;

        Ok(Self { con })
    }

    /*~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~/
    /                                 INITIALIZATION                                 /
    /~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~*/

    /// This function assumes that the FIP number is not already in the database
    pub fn new_vote(
        &mut self,
        fip_number: impl Into<u32>,
        vote: Option<Vote>,
    ) -> Result<(), RedisError> {
        // If vote is None, set the vector to empty
        let vote = match vote {
            Some(v) => vec![v],
            None => vec![],
        };

        let fip_num = fip_number.into();

        let vote_key = LookupKey::FipNumber(fip_num).to_bytes();
        let time_key = LookupKey::Timestamp(fip_num).to_bytes();

        // Set a map of FIP number to vector of all votes
        self.con.set::<Vec<u8>, Vec<Vote>, ()>(vote_key, vote)?;

        // Set a map of FIP to timestamp of vote start
        let timestamp = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        self.con.set::<Vec<u8>, u64, ()>(time_key, timestamp)?;

        Ok(())
    }

    /*~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~/
    /                                     GETTERS                                    /
    /~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~*/

    fn votes(&mut self, fip_number: impl Into<u32>) -> Result<Vec<Vote>, RedisError> {
        let key = LookupKey::FipNumber(fip_number.into()).to_bytes();
        let votes: Vec<Vote> = self.con.get::<Vec<u8>, Vec<Vote>>(key)?;
        Ok(votes)
    }

    pub fn vote_results(&mut self, fip_number: impl Into<u32>) -> Result<String, RedisError> {
        let mut yay = 0;
        let mut nay = 0;
        let mut abstain = 0;

        let votes = self.votes(fip_number)?;

        for vote in votes {
            match vote.choice {
                VoteOption::Yay => yay += 1,
                VoteOption::Nay => nay += 1,
                VoteOption::Abstain => abstain += 1,
            }
        }

        let results = VoteResults {
            yay,
            nay,
            abstain,
            yay_storage_size: 0,
            nay_storage_size: 0,
            abstain_storage_size: 0,
        };

        match serde_json::to_string(&results) {
            Ok(j) => Ok(j),
            Err(_) => Err(RedisError::from((
                redis::ErrorKind::TypeError,
                "Error serializing vote results",
            ))),
        }
    }

    pub fn vote_status(&mut self, fip_number: impl Into<u32>, vote_length: impl Into<u64>) -> Result<VoteStatus, RedisError> {
        let num = fip_number.into();
        let vote_key = LookupKey::FipNumber(num).to_bytes();
        let time_key = LookupKey::Timestamp(num).to_bytes();

        // Check if the FIP number exists in the database
        if !self.con.exists(vote_key)? {
            return Ok(VoteStatus::DoesNotExist);
        }

        // Check if the FIP number has a timestamp
        if !self.con.exists(time_key.clone())? {
            return Ok(VoteStatus::DoesNotExist);
        }

        // Check if the vote is still open
        let time_start: u64 = self.vote_start(num)?;
        let now = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        if now - time_start < vote_length.into() {
            return Ok(VoteStatus::Open);
        } else {
            return Ok(VoteStatus::Closed);
        }
    }

    fn vote_start(&mut self, fip_number: impl Into<u32>) -> Result<u64, RedisError> {
        let key = LookupKey::Timestamp(fip_number.into()).to_bytes();
        let timestamp: u64 = self.con.get::<Vec<u8>, u64>(key)?;
        Ok(timestamp)
    }

    /*~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~/
    /                                     SETTERS                                    /
    /~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~*/

    pub fn add_vote<T>(&mut self, fip_number: T, vote: Vote) -> Result<(), RedisError>
    where
        T: Into<u32>,
    {
        let num: u32 = fip_number.into();

        if self.votes(num)?.is_empty() {
            self.new_vote(num, Some(vote))?;
            return Ok(());
        }

        let mut votes: Vec<Vote> = self.con.get::<u32, Vec<Vote>>(num)?;
        votes.push(vote);
        self.con.set::<u32, Vec<Vote>, ()>(num, votes)?;
        Ok(())
    }
}
