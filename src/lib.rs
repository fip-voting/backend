pub mod redis;
pub mod storage;
pub mod votes;
pub mod vote_registration;

use std::str::FromStr;

use actix_web::{get, post, web, HttpResponse, Responder};
use clap::{arg, command, Parser};
use ethers::types::Address;
use serde::Deserialize;
use url::Url;

use crate::{vote_registration::ReceivedVoterRegistration, storage::Network};

use {
    crate::redis::{Redis, VoteStatus},
    votes::ReceivedVote,
};

// Error messages
const OPEN_CONNECTION_ERROR: &str = "Error opening connection to in-memory database";
const VOTE_STATUS_ERROR: &str = "Error getting vote status";
const VOTE_RESULTS_ERROR: &str = "Error getting vote results";
const VOTE_DESERIALIZE_ERROR: &str = "Error deserializing vote";
const VOTE_RECOVER_ERROR: &str = "Error recovering vote";
const VOTE_ADD_ERROR: &str = "Error adding vote";
const INVALID_NETWORK: &str = "Invalid network";
const INVALID_ADDRESS: &str = "Invalid address";

// Default values for command line arguments
const VOTE_LENGTH: &str = "60";
const REDIS_DEFAULT_PATH: &str = "redis://127.0.0.1:6379";
const DEFAULT_SERVE_ADDRESS: &str = "http://127.0.0.1:51634";

#[derive(Parser, Clone)]
#[command(name = "filecoin-vote")]
pub struct Args {
    #[arg(short, long, default_value = DEFAULT_SERVE_ADDRESS)]
    pub serve_address: Url,
    #[arg(short, long, default_value = REDIS_DEFAULT_PATH)]
    pub redis_path: Url,
    #[arg(short, long, default_value = VOTE_LENGTH)]
    pub vote_length: u64,
}

impl Args {
    pub fn new() -> Self {
        Self::parse()
    }

    pub fn vote_length(&self) -> u64 {
        self.vote_length
    }

    pub fn redis_path(&self) -> Url {
        self.redis_path.clone()
    }

    pub fn serve_address(&self) -> Url {
        self.serve_address.clone()
    }
}

#[derive(Deserialize)]
struct NtwParams {
    network: String,
    fip_number: u32,
}

#[derive(Deserialize)]
struct DelegateParams {
    network: String,
    address: String,
}

#[derive(Deserialize)]
struct FipParams {
    fip_number: u32,
}

#[get("/filecoin/vote")]
async fn get_votes(query_params: web::Query<NtwParams>, config: web::Data<Args>) -> impl Responder {
    println!("votes requested");

    let ntw = match query_params.network.as_str() {
        "mainnet" => Network::Mainnet,
        "calibration" => Network::Testnet,
        _ => return HttpResponse::BadRequest().body(INVALID_NETWORK),
    };
    let num = query_params.fip_number;

    // Open a connection to the redis database
    let mut redis = match Redis::new(config.redis_path()) {
        Ok(redis) => redis,
        Err(e) => {
            println!("{}", e);
            return HttpResponse::InternalServerError().body(OPEN_CONNECTION_ERROR);
        }
    };

    // Get the status of the vote from the database
    let status = match redis.vote_status(num, config.vote_length(), ntw) {
        Ok(status) => status,
        Err(e) => {
            println!("{}", e);
            return HttpResponse::InternalServerError().body(VOTE_STATUS_ERROR);
        }
    };

    println!("Vote status: {:?} for FIP: {}", status, num);

    // Return the appropriate response
    match status {
        VoteStatus::InProgress => HttpResponse::Forbidden().finish(),
        VoteStatus::Concluded => {
            let vote_results = match redis.vote_results(num, ntw) {
                Ok(results) => results,
                Err(e) => {
                    println!("{}", e);
                    return HttpResponse::InternalServerError().body(VOTE_RESULTS_ERROR);
                }
            };
            HttpResponse::Ok().json(vote_results)
        }
        VoteStatus::DoesNotExist => HttpResponse::NotFound().finish(),
    }
}

#[post("/filecoin/vote")]
async fn register_vote(
    body: web::Bytes,
    query_params: web::Query<FipParams>,
    config: web::Data<Args>,
) -> impl Responder {
    let num = query_params.fip_number;

    println!("Vote received for FIP: {}", num);
    // Deserialize the body into the vote struct
    let vote: ReceivedVote = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            println!("{}", e);
            return HttpResponse::BadRequest().body(VOTE_DESERIALIZE_ERROR);
        }
    };

    // Recover the vote
    let vote = match vote.vote() {
        Ok(vote) => vote,
        Err(e) => {
            println!("{}", e);
            return HttpResponse::BadRequest().body(VOTE_RECOVER_ERROR);
        }
    };

    let voter = vote.voter();

    // Open a connection to the redis database
    let mut redis = match Redis::new(config.redis_path()) {
        Ok(redis) => redis,
        Err(e) => {
            println!("{}", e);
            return HttpResponse::InternalServerError().body(OPEN_CONNECTION_ERROR);
        }
    };

    let ntw = match redis.network(voter) {
        Ok(ntw) => ntw,
        Err(e) => {
            println!("{}", e);
            return HttpResponse::InternalServerError().body(INVALID_ADDRESS);
        }
    };

    let status = match redis.vote_status(num, config.vote_length(), ntw) {
        Ok(status) => status,
        Err(e) => {
            println!("{}", e);
            return HttpResponse::InternalServerError().body(VOTE_STATUS_ERROR);
        }
    };

    match status {
        VoteStatus::InProgress => (),
        VoteStatus::Concluded => {
            println!("Vote concluded for FIP: {}", num);
            return HttpResponse::Forbidden().finish();
        }
        VoteStatus::DoesNotExist => (),
    }

    let choice = vote.choice();

    // Add the vote to the database
    match redis.add_vote(num, vote, voter).await {
        Ok(_) => (),
        Err(e) => {
            println!("{}", e);
            return HttpResponse::InternalServerError().body(VOTE_ADD_ERROR);
        }
    }

    println!(
        "Vote ({:?}) added for FIP: {}",
        choice, num
    );

    HttpResponse::Ok().finish()
}

#[post("/filecoin/register")]
async fn register_voter(body: web::Bytes, config: web::Data<Args>) -> impl Responder {
    println!("Voter registration received");

    // Deserialize the body into the vote struct
    let reg: ReceivedVoterRegistration = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            println!("{}", e);
            return HttpResponse::BadRequest().body(VOTE_DESERIALIZE_ERROR);
        }
    };

    let registration = match reg.recover_vote_registration().await {
        Ok(registration) => registration,
        Err(e) => {
            println!("{}", e);
            return HttpResponse::BadRequest().body(VOTE_RECOVER_ERROR);
        }
    };

    // Open a connection to the redis database
    let mut redis = match Redis::new(config.redis_path()) {
        Ok(redis) => redis,
        Err(e) => {
            println!("{}", e);
            return HttpResponse::InternalServerError().body(OPEN_CONNECTION_ERROR);
        }
    };

    // Add the vote to the database
    match redis.register_voter(registration) {
        Ok(_) => (),
        Err(e) => {
            println!("{}", e);
            return HttpResponse::InternalServerError().body(VOTE_ADD_ERROR);
        }
    }

    HttpResponse::Ok().finish()
}

#[get("/filecoin/delegates")]
async fn get_delegates(query_params: web::Query<DelegateParams>, config: web::Data<Args>) -> impl Responder {
    println!("Delegates requested");

    let ntw = match query_params.network.as_str() {
        "mainnet" => Network::Mainnet,
        "calibration" => Network::Testnet,
        _ => return HttpResponse::BadRequest().body(INVALID_NETWORK),
    };
    let address = query_params.address.clone();

    let address = match Address::from_str(address.as_str()) {
        Ok(address) => address,
        Err(e) => {
            println!("{}", e);
            return HttpResponse::BadRequest().body(INVALID_ADDRESS);
        }
    };

    // Open a connection to the redis database
    let mut redis = match Redis::new(config.redis_path()) {
        Ok(redis) => redis,
        Err(e) => {
            println!("{}", e);
            return HttpResponse::InternalServerError().body(OPEN_CONNECTION_ERROR);
        }
    };

    // Get the status of the vote from the database
    let delegates = match redis.voter_delegates(address, ntw) {
        Ok(delegates) => delegates,
        Err(e) => {
            println!("{}", e);
            return HttpResponse::InternalServerError().body(VOTE_STATUS_ERROR);
        }
    };

    println!("Delegates: {:?} for address: {}", delegates, address);

    let mut dgts: Vec<String> = Vec::new();
    let prefix = match ntw {
        Network::Mainnet => "f",
        Network::Testnet => "t",
    };
    for delegate in delegates {
        dgts.push(format!("{}0{}", prefix, delegate.to_string()));
    }

    HttpResponse::Ok().json(dgts)
}