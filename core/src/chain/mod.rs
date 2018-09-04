pub mod actor;
pub mod header;
pub mod pair;

use actor::Protocol;
use chain::{
    actor::{AskChain, ChainActor},
    header::Header,
    pair::Pair,
};
use error::HolochainError;
use hash_table::{entry::Entry, sys_entry::ToEntry, HashTable};
use json::ToJson;
use key::Key;
use riker::actors::*;
use serde_json;

/// Iterator type for pairs in a chain
/// next method may panic if there is an error in the underlying table
#[derive(Clone)]
pub struct ChainIterator {
    table_actor: ActorRef<Protocol>,
    current: Option<Pair>,
}

impl ChainIterator {
    #[allow(unknown_lints)]
    #[allow(needless_pass_by_value)]
    pub fn new(table: ActorRef<Protocol>, pair: &Option<Pair>) -> ChainIterator {
        ChainIterator {
            current: pair.clone(),
            table_actor: table.clone(),
        }
    }
}

impl Iterator for ChainIterator {
    type Item = Pair;

    /// May panic if there is an underlying error in the table
    fn next(&mut self) -> Option<Pair> {
        let previous = self.current.take();

        self.current = previous.as_ref()
                        .and_then(|p| p.header().link())
                        // @TODO should this panic?
                        // @see https://github.com/holochain/holochain-rust/issues/146
                        .and_then(|h| {
                let header_entry = &self.table_actor.entry(&h.to_string())
                                    .expect("getting from a table shouldn't fail")
                                    .expect("getting from a table shouldn't fail");
                // Recreate the Pair from the HeaderEntry
                let header = Header::from_entry(header_entry);
                let pair = Pair::from_header(&self.table_actor, &header);
                pair
                        });
        previous
    }
}

#[derive(Clone, Debug)]
pub struct Chain {
    chain_actor: ActorRef<Protocol>,
    table_actor: ActorRef<Protocol>,
}

impl PartialEq for Chain {
    // @TODO can we just check the actors are equal? is actor equality a thing?
    // @see https://github.com/holochain/holochain-rust/issues/257
    fn eq(&self, other: &Chain) -> bool {
        // an invalid chain is like NaN... not even equal to itself
        self.validate() &&
        other.validate() &&
        // header hashing ensures that if the tops match the whole chain matches
        self.top_pair() == other.top_pair()
    }
}

impl Eq for Chain {}

/// Turns a chain into an iterator over it's Pairs
impl IntoIterator for Chain {
    type Item = Pair;
    type IntoIter = ChainIterator;

    /// returns a ChainIterator that provides cloned Pairs from the underlying HashTable
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl Chain {
    pub fn new(table: ActorRef<Protocol>) -> Chain {
        Chain {
            chain_actor: ChainActor::new_ref(),
            table_actor: table.clone(),
        }
    }

    /// Create the next commitable Header for the chain.
    /// a Header is immutable, but the chain is mutable if chain.commit_*() is used.
    /// this means that a header becomes invalid and useless as soon as the chain is mutated
    /// the only valid usage of a header is to immediately commit it onto a chain in a Pair.
    /// normally (outside unit tests) the generation of valid headers is internal to the
    /// chain::SourceChain trait and should not need to be handled manually
    ///
    /// @see chain::pair::Pair
    /// @see chain::entry::Entry
    pub fn create_next_header(&self, entry: &Entry) -> Header {
        Header::new(
            &entry.entry_type().clone(),
            // @TODO implement timestamps
            // https://github.com/holochain/holochain-rust/issues/70
            &String::new(),
            self.top_pair()
                .as_ref()
                .map(|p| p.header().to_entry().key()),
            &entry.hash().to_string(),
            // @TODO implement signatures
            // https://github.com/holochain/holochain-rust/issues/71
            &String::new(),
            self
                .top_pair_of_type(&entry.entry_type())
                // @TODO inappropriate expect()?
                // @see https://github.com/holochain/holochain-rust/issues/147
                .map(|p| p.header().hash()),
        )
    }

    /// Create the next commitable Pair for this chain
    ///
    /// Header is generated
    ///
    /// a Pair is immutable, but the chain is mutable if chain.commit_*() is used.
    ///
    /// this means that if two Pairs X and Y are generated for chain C then Pair X is pushed onto
    /// C to create chain C' (containing X), then Pair Y is no longer valid as the headers would
    /// need to include X. Pair Y can be regenerated with the same parameters as Y' and will be
    /// now be valid, the new Y' will include correct headers pointing to X.
    ///
    /// # Panics
    ///
    /// Panics if entry is somehow invalid
    ///
    /// @see chain::entry::Entry
    /// @see chain::header::Header
    pub fn create_next_pair(&self, entry: &Entry) -> Pair {
        let new_pair = Pair::new(&self.create_next_header(entry), &entry.clone());

        // we panic as no code path should attempt to create invalid pairs
        // creating a Pair is an internal process of chain.push() and is deterministic based on
        // an immutable Entry (that itself cannot be invalid), so this should never happen.
        assert!(new_pair.validate(), "attempted to create an invalid pair");

        new_pair
    }

    /// returns true if all pairs in the chain pass validation
    fn validate(&self) -> bool {
        self.iter().all(|p| p.validate())
    }

    /// returns a ChainIterator that provides cloned Pairs from the underlying HashTable
    fn iter(&self) -> ChainIterator {
        ChainIterator::new(self.table(), &self.top_pair())
    }

    /// restore canonical JSON chain
    /// can't implement json::FromJson due to Chain's need for a table actor
    /// @TODO accept canonical JSON
    /// @see https://github.com/holochain/holochain-rust/issues/75
    pub fn from_json(table: ActorRef<Protocol>, s: &str) -> Self {
        // @TODO inappropriate unwrap?
        // @see https://github.com/holochain/holochain-rust/issues/168
        let mut as_seq: Vec<Pair> = serde_json::from_str(s).expect("argument should be valid json");
        as_seq.reverse();

        let mut chain = Chain::new(table);

        for p in as_seq {
            chain.commit_pair(&p).expect("pair should be valid");
        }
        chain
    }

    /// table getter
    /// returns a reference to the underlying HashTable
    pub fn table(&self) -> ActorRef<Protocol> {
        self.table_actor.clone()
    }
}

// @TODO should SourceChain have a bound on HashTable for consistency?
// @see https://github.com/holochain/holochain-rust/issues/261
pub trait SourceChain {
    /// sets an option for the top Pair
    fn set_top_pair(&self, &Option<Pair>) -> Result<Option<Pair>, HolochainError>;
    /// returns an option for the top Pair
    fn top_pair(&self) -> Option<Pair>;
    /// get the top Pair by Entry type
    fn top_pair_of_type(&self, t: &str) -> Option<Pair>;

    /// push a new Entry on to the top of the Chain.
    /// The Pair for the new Entry is generated and validated against the current top
    /// Pair to ensure the chain links up correctly across the underlying table data
    /// the newly created and pushed Pair is returned.
    fn commit_entry(&mut self, entry: &Entry) -> Result<Pair, HolochainError>;
    /// get an Entry by Entry key from the HashTable if it exists
    fn entry(&self, entry_hash: &str) -> Option<Entry>;

    /// pair-oriented version of push_entry()
    fn commit_pair(&mut self, pair: &Pair) -> Result<Pair, HolochainError>;
    /// get a Pair by Pair/Header key from the HashTable if it exists
    fn pair(&self, pair_hash: &str) -> Option<Pair>;
}

impl SourceChain for Chain {
    fn top_pair(&self) -> Option<Pair> {
        self.chain_actor.top_pair()
    }

    fn set_top_pair(&self, pair: &Option<Pair>) -> Result<Option<Pair>, HolochainError> {
        self.chain_actor.set_top_pair(&pair)
    }

    fn top_pair_of_type(&self, t: &str) -> Option<Pair> {
        self.iter().find(|p| p.header().entry_type() == t)
    }

    /// Whole process of authoring an entry.
    /// 1. `validation` of the new entry using the ribosome and validation WASM code
    /// 2. `pushing` the new entry onto the source chain, if valid
    /// 3. `putting` the entry into the (distributed) hash table, if defined as public
    fn commit_pair(&mut self, pair: &Pair) -> Result<Pair, HolochainError> {
        // 1. validation
        if !(pair.validate()) {
            return Err(HolochainError::new(
                "attempted to push an invalid pair for this chain",
            ));
        }

        let top_pair = self.top_pair().as_ref().map(|p| p.key());
        let prev_pair = pair.header().link();

        if top_pair != prev_pair {
            return Err(HolochainError::new(&format!(
                "top pair did not match previous hash pair from commited pair: {:?} vs. {:?}",
                top_pair, prev_pair,
            )));
        }

        // 2. pushing
        // 3. putting
        let header_entry = &pair.clone().header().to_entry();
        // println!("Chain.commit_pair() header_entry = {:?}", header_entry);
        self.table_actor.put_entry(header_entry)?;
        self.table_actor.put_entry(&pair.clone().entry())?;

        // 4. Mutate Chain accordingly
        // @TODO instead of unwrapping this, move all the above validation logic inside of
        // set_top_pair()
        // @see https://github.com/holochain/holochain-rust/issues/258
        // @TODO if top pair set fails but commit succeeds?
        // @see https://github.com/holochain/holochain-rust/issues/259
        self.set_top_pair(&Some(pair.clone()))?;

        // Done
        Ok(pair.clone())
    }

    fn commit_entry(&mut self, entry: &Entry) -> Result<Pair, HolochainError> {
        let pair = self.create_next_pair(entry);
        self.commit_pair(&pair)
    }

    /// Browse Chain until Pair is found
    fn pair(&self, pair_hash: &str) -> Option<Pair> {
        // @TODO - this is a slow way to do a lookup
        // @see https://github.com/holochain/holochain-rust/issues/50
        self
            .iter()
            // @TODO entry hashes are NOT unique across pairs so k/v lookups can't be 1:1
            // @see https://github.com/holochain/holochain-rust/issues/145
            .find(|p| {
                p.key() == pair_hash
            })
    }

    /// Browse Chain until Pair with entry_hash is found
    fn entry(&self, entry_hash: &str) -> Option<Entry> {
        // @TODO - this is a slow way to do a lookup
        // @see https://github.com/holochain/holochain-rust/issues/50
        let pair = self
                .iter()
                // @TODO entry hashes are NOT unique across pairs so k/v lookups can't be 1:1
                // @see https://github.com/holochain/holochain-rust/issues/145
            .find(|p| {
                p.entry().hash() == entry_hash
            });
        if pair.is_none() {
            return None;
        };
        Some(pair.unwrap().entry().clone())
    }
}

impl ToJson for Chain {
    /// get the entire chain, top to bottom as a JSON array or canonical pairs
    /// @TODO return canonical JSON
    /// @see https://github.com/holochain/holochain-rust/issues/75
    fn to_json(&self) -> Result<String, HolochainError> {
        let as_seq = self.iter().collect::<Vec<Pair>>();
        Ok(serde_json::to_string(&as_seq)?)
    }
}

#[cfg(test)]
pub mod tests {

    use super::Chain;
    use chain::{
        pair::{tests::test_pair, Pair},
        SourceChain,
    };
    use hash_table::{
        actor::tests::test_table_actor,
        entry::tests::{test_entry, test_entry_a, test_entry_b, test_type_a, test_type_b},
        HashTable,
    };
    use json::ToJson;
    use key::Key;
    use std::thread;

    /// builds a dummy chain for testing
    pub fn test_chain() -> Chain {
        Chain::new(test_table_actor())
    }

    #[test]
    /// smoke test for new chains
    fn new() {
        test_chain();
    }

    #[test]
    /// test chain equality
    fn eq() {
        let mut chain1 = test_chain();
        let mut chain2 = test_chain();
        let mut chain3 = test_chain();

        let entry_a = test_entry_a();
        let entry_b = test_entry_b();

        chain1
            .commit_entry(&entry_a)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");
        chain2
            .commit_entry(&entry_a)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");
        chain3
            .commit_entry(&entry_b)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");

        assert_eq!(chain1.top_pair(), chain2.top_pair());
        assert_eq!(chain1, chain2);

        assert_ne!(chain1, chain3);
        assert_ne!(chain2, chain3);
    }

    #[test]
    /// tests for chain.top_pair()
    fn top_pair() {
        let mut chain = test_chain();

        assert_eq!(None, chain.top_pair());

        let entry_a = test_entry_a();
        let entry_b = test_entry_b();

        let p1 = chain
            .commit_entry(&entry_a)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");
        assert_eq!(&entry_a, p1.entry());
        let top_pair = chain.top_pair().expect("should have commited entry");
        assert_eq!(p1, top_pair);

        let p2 = chain
            .commit_entry(&entry_b)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");
        assert_eq!(&entry_b, p2.entry());
        let top_pair = chain.top_pair().expect("should have commited entry");
        assert_eq!(p2, top_pair);
    }

    #[test]
    /// tests that the chain state is consistent across clones
    fn clone_safe() {
        let c1 = test_chain();
        let mut c2 = c1.clone();
        let test_pair = test_pair();

        assert_eq!(None, c1.top_pair());
        assert_eq!(None, c2.top_pair());

        let pair = c2.commit_pair(&test_pair).unwrap();

        assert_eq!(Some(pair.clone()), c2.top_pair());
        assert_eq!(c1.top_pair(), c2.top_pair());
    }

    #[test]
    // test that adding something to the chain adds to the table
    fn table_put() {
        let table_actor = test_table_actor();
        let mut chain = Chain::new(table_actor.clone());

        let pair = chain
            .commit_pair(&test_pair())
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");

        let table_entry = table_actor
            .entry(&pair.entry().key())
            .expect("getting an entry from a table in a chain shouldn't fail")
            .expect("table should have entry");
        let chain_entry = chain
            .entry(&pair.entry().key())
            .expect("getting an entry from a chain shouldn't fail");

        assert_eq!(pair.entry(), &table_entry);
        assert_eq!(table_entry, chain_entry);
    }

    #[test]
    fn can_commit_entry() {
        let mut chain = test_chain();

        assert_eq!(None, chain.top_pair());

        // chain top, pair entry and headers should all line up after a push
        let e1 = test_entry_a();
        let p1 = chain
            .commit_entry(&e1)
            .expect("pushing a valid entry to an exclusively owned chain shouldn't fail");

        assert_eq!(&e1, p1.entry());
        assert_eq!(Some(&p1), chain.top_pair().as_ref());
        assert_eq!(e1.key(), p1.entry().key());

        // we should be able to do it again
        let e2 = test_entry_b();
        let p2 = chain
            .commit_entry(&e2)
            .expect("pushing a valid entry to an exclusively owned chain shouldn't fail");

        assert_eq!(&e2, p2.entry());
        assert_eq!(Some(&p2), chain.top_pair().as_ref());
        assert_eq!(e2.key(), p2.entry().key());
    }

    #[test]
    fn validate() {
        println!("can_validate: Empty Chain");
        let mut chain = test_chain();
        assert!(chain.validate());

        println!("can_validate: Chain One");
        let e1 = test_entry_a();
        chain
            .commit_entry(&e1)
            .expect("pushing a valid entry to an exclusively owned chain shouldn't fail");
        assert!(chain.validate());

        println!("can_validate: Chain with Two");
        let e2 = test_entry_b();
        chain
            .commit_entry(&e2)
            .expect("pushing a valid entry to an exclusively owned chain shouldn't fail");
        assert!(chain.validate());
    }

    #[test]
    /// test chain.push() and chain.get() together
    fn round_trip() {
        let mut chain = test_chain();
        let entry = test_entry();
        let pair = chain
            .commit_entry(&entry)
            .expect("pushing a valid entry to an exclusively owned chain shouldn't fail");

        assert_eq!(
            entry,
            chain
                .entry(&pair.entry().key())
                .expect("getting an entry from a chain shouldn't fail"),
        );
    }

    #[test]
    /// show that we can push the chain a bit without issues e.g. async
    fn round_trip_stress_test() {
        let h = thread::spawn(|| {
            let mut chain = test_chain();
            let entry = test_entry();

            for _ in 1..100 {
                let pair = chain.commit_entry(&entry).unwrap();
                assert_eq!(Some(pair.entry().clone()), chain.entry(&pair.entry().key()),);
            }
        });
        h.join().unwrap();
    }

    #[test]
    /// test chain.iter()
    fn iter() {
        let mut chain = test_chain();

        let e1 = test_entry_a();
        let e2 = test_entry_b();

        let p1 = chain
            .commit_entry(&e1)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");
        let p2 = chain
            .commit_entry(&e2)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");

        assert_eq!(vec![p2, p1], chain.iter().collect::<Vec<Pair>>());
    }

    #[test]
    /// test chain.iter() functional interface
    fn iter_functional() {
        let mut chain = test_chain();

        let e1 = test_entry_a();
        let e2 = test_entry_b();

        let p1 = chain
            .commit_entry(&e1)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");
        let _p2 = chain
            .commit_entry(&e2)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");
        let p3 = chain
            .commit_entry(&e1)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");

        assert_eq!(
            vec![p3, p1],
            chain
                .iter()
                .filter(|p| p.entry().entry_type() == "testEntryType")
                .collect::<Vec<Pair>>()
        );
    }

    #[test]
    fn entry_advance() {
        let mut chain = test_chain();

        let e1 = test_entry_a();
        let e2 = test_entry_b();

        let p1 = chain
            .commit_entry(&e1)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");
        let p2 = chain
            .commit_entry(&e2)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");

        assert_eq!(
            p1.entry().clone(),
            chain
                .entry(&p1.entry().key())
                .expect("getting an entry from a chain shouldn't fail"),
        );

        let p3 = chain
            .commit_entry(&e1)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");

        assert_eq!(None, chain.entry(""));
        assert_eq!(
            p3.entry().clone(),
            chain
                .entry(&p1.entry().key())
                .expect("getting an entry from a chain shouldn't fail"),
        );
        assert_eq!(
            p2.entry().clone(),
            chain
                .entry(&p2.entry().key())
                .expect("getting an entry from a chain shouldn't fail"),
        );
        assert_eq!(
            p3.entry().clone(),
            chain
                .entry(&p3.entry().key())
                .expect("getting an entry from a chain shouldn't fail"),
        );

        assert_eq!(
            p1,
            chain
                .pair(&p1.key())
                .expect("getting an entry from a chain shouldn't fail"),
        );
        assert_eq!(
            p2,
            chain
                .pair(&p2.key())
                .expect("getting an entry from a chain shouldn't fail"),
        );
        assert_eq!(
            p3,
            chain
                .pair(&p3.key())
                .expect("getting an entry from a chain shouldn't fail"),
        );
    }

    #[test]
    fn entry() {
        let mut chain = test_chain();

        let e1 = test_entry_a();
        let e2 = test_entry_b();

        let p1 = chain
            .commit_entry(&e1)
            .expect("pushing a valid entry to an exclusively owned chain shouldn't fail");
        let p2 = chain
            .commit_entry(&e2)
            .expect("pushing a valid entry to an exclusively owned chain shouldn't fail");
        let p3 = chain
            .commit_entry(&e1)
            .expect("pushing a valid entry to an exclusively owned chain shouldn't fail");

        assert_eq!(None, chain.entry(""));
        // @TODO at this point we have p3 with the same entry key as p1...
        assert_eq!(
            p3.entry().clone(),
            chain
                .entry(&p1.entry().key())
                .expect("getting an entry from a chain shouldn't fail"),
        );
        assert_eq!(
            p2.entry().clone(),
            chain
                .entry(&p2.entry().key())
                .expect("getting an entry from a chain shouldn't fail"),
        );
        assert_eq!(
            p3.entry().clone(),
            chain
                .entry(&p3.entry().key())
                .expect("getting an entry from a chain shouldn't fail"),
        );
    }

    #[test]
    fn top_pair_of_type() {
        let mut chain = test_chain();

        assert_eq!(None, chain.top_pair_of_type(&test_type_a()));
        assert_eq!(None, chain.top_pair_of_type(&test_type_b()));

        let entry1 = test_entry_a();
        let entry2 = test_entry_b();

        // type a should be p1
        // type b should be None
        let pair1 = chain
            .commit_entry(&entry1)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");
        assert_eq!(
            Some(&pair1),
            chain.top_pair_of_type(&test_type_a()).as_ref()
        );
        assert_eq!(None, chain.top_pair_of_type(&test_type_b()));

        // type a should still be pair1
        // type b should be p2
        let pair2 = chain
            .commit_entry(&entry2)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");
        assert_eq!(
            Some(&pair1),
            chain.top_pair_of_type(&test_type_a()).as_ref()
        );
        assert_eq!(
            Some(&pair2),
            chain.top_pair_of_type(&test_type_b()).as_ref()
        );

        // type a should be pair3
        // type b should still be pair2
        let pair3 = chain
            .commit_entry(&entry1)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");

        assert_eq!(
            Some(&pair3),
            chain.top_pair_of_type(&test_type_a()).as_ref()
        );
        assert_eq!(
            Some(&pair2),
            chain.top_pair_of_type(&test_type_b()).as_ref()
        );
    }

    #[test]
    /// test IntoIterator implementation
    fn into_iter() {
        let mut chain = test_chain();

        let e1 = test_entry_a();
        let e2 = test_entry_b();

        let p1 = chain
            .commit_entry(&e1)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");
        let p2 = chain
            .commit_entry(&e2)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");
        let p3 = chain
            .commit_entry(&e1)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");

        // into_iter() returns clones of pairs
        assert_eq!(vec![p3, p2, p1], chain.into_iter().collect::<Vec<Pair>>());
    }

    #[test]
    /// test to_json() and from_json() implementation
    fn json_round_trip() {
        let mut chain = test_chain();

        let e1 = test_entry_a();
        let e2 = test_entry_b();

        chain
            .commit_entry(&e1)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");
        chain
            .commit_entry(&e2)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");
        chain
            .commit_entry(&e1)
            .expect("pushing a valid entry to an exlusively owned chain shouldn't fail");

        let expected_json = "[{\"header\":{\"entry_type\":\"testEntryType\",\"timestamp\":\"\",\"link\":\"QmdEVL9whBj1Tr9VoR6BzmVjrgyPdN5vJ2bbdQdwwfQ9Uq\",\"entry_hash\":\"QmbXSE38SN3SuJDmHKSSw5qWWegvU7oTxrLDRavWjyxMrT\",\"entry_signature\":\"\",\"link_same_type\":\"QmawqBCVVap9KdaakqEHF4JzUjjLhmR7DpM5jgJko8j1rA\"},\"entry\":{\"content\":\"test entry content\",\"entry_type\":\"testEntryType\"}},{\"header\":{\"entry_type\":\"testEntryTypeB\",\"timestamp\":\"\",\"link\":\"QmU8vuUfCQGBb8SUdWjKqmSmsWwXBn4AJPb3HLb8cqWtYn\",\"entry_hash\":\"QmPz5jKXsxq7gPVAbPwx5gD2TqHfqB8n25feX5YH18JXrT\",\"entry_signature\":\"\",\"link_same_type\":null},\"entry\":{\"content\":\"other test entry content\",\"entry_type\":\"testEntryTypeB\"}},{\"header\":{\"entry_type\":\"testEntryType\",\"timestamp\":\"\",\"link\":null,\"entry_hash\":\"QmbXSE38SN3SuJDmHKSSw5qWWegvU7oTxrLDRavWjyxMrT\",\"entry_signature\":\"\",\"link_same_type\":null},\"entry\":{\"content\":\"test entry content\",\"entry_type\":\"testEntryType\"}}]"
        ;
        assert_eq!(
            expected_json,
            chain.to_json().expect("chain shouldn't fail to serialize")
        );

        let table_actor = test_table_actor();
        assert_eq!(chain, Chain::from_json(table_actor, expected_json));
    }

}
