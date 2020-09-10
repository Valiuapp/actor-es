use crate::{EntityId, Event, EventBus, Model};
use async_trait::async_trait;
use chrono::prelude::*;
use futures::future::ok;
use futures::stream::{BoxStream, StreamExt, TryStreamExt};
use riker::actors::*;
use std::fmt;
use std::ops::Deref;
use thiserror::Error;

pub use in_memory::MemStore;

mod in_memory;

#[async_trait]
pub trait CommitStore<M: Model>: fmt::Debug + Clone + Send + Sync + 'static {
    fn keys(&self) -> BoxStream<CommitResult<EntityId>>;

    fn change_list(&self, id: EntityId) -> BoxStream<CommitResult<Commit<M>>>;

    async fn commit(&self, c: Commit<M>) -> CommitResult<()>;

    fn entities(&self) -> BoxStream<CommitResult<TimeTraveler<'_, M>>> {
        self.keys().and_then(move |id| self.get(id)).boxed()
    }

    async fn get(&self, id: EntityId) -> CommitResult<TimeTraveler<'_, M>> {
        let mut changes = self.change_list(id);
        let model = changes
            .try_next()
            .await?
            .ok_or(CommitError::NotFound)?
            .entity()
            // first change has to be the entity
            .unwrap();
        Ok(TimeTraveler { changes, model })
    }

    async fn snapshot(&self, id: EntityId, time: DateTime<Utc>) -> CommitResult<M> {
        self.get(id).await?.travel_to(time).await
    }
}

pub type CommitResult<T> = Result<T, CommitError>;

#[derive(Error, Clone, Debug)]
pub enum CommitError {
    #[error("Cant change non existing entity")]
    CantChange,
    #[error("Didn't find commit for entity")]
    NotFound,
}

/// A wrapper for a stored entity that applies changes until the specified moment in time.
pub struct TimeTraveler<'a, M: Model> {
    model: M,
    changes: BoxStream<'a, CommitResult<Commit<M>>>,
}

impl<'a, M: Model> TimeTraveler<'a, M> {
    pub async fn to_present(self) -> CommitResult<M> {
        self.travel_to(Utc::now()).await
    }

    pub async fn travel_to(self, _until: DateTime<Utc>) -> CommitResult<M> {
        let model = self
            .changes
            .try_fold(self.model, |mut m, c| {
                let change = c.change().unwrap();
                m.apply_change(&change);
                ok(m)
            })
            .await?;
        Ok(model)
    }
}

impl<M: Model> fmt::Debug for TimeTraveler<'_, M> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "TimeTraveler({:?})", self.model)
    }
}

/// An actor that handles the persistance of changes of an entity
/// using "commits" to track who made the change and why.  
#[derive(Debug)]
pub struct Store<M: Model, S: CommitStore<M>> {
    bus: Option<EventBus<M>>,
    backend: S,
}

pub type StoreRef<A> = ActorRef<StoreMsg<A>>;

impl<M, S> Actor for Store<M, S>
where
    M: Model,
    S: CommitStore<M>,
{
    type Msg = StoreMsg<M>;

    fn recv(&mut self, cx: &Context<Self::Msg>, msg: Self::Msg, sender: Sender) {
        match msg {
            StoreMsg::Commit(msg) => self.receive(cx, msg, sender),
            StoreMsg::Subscribe(msg) => self.receive(cx, msg, sender),
            StoreMsg::Snapshot(msg) => self.receive(cx, msg, sender),
            StoreMsg::SnapshotList(msg) => self.receive(cx, msg, sender),
        };
    }
}

impl<M, S> ActorFactoryArgs<S> for Store<M, S>
where
    M: Model,
    S: CommitStore<M>,
{
    fn create_args(backend: S) -> Self {
        Store { backend, bus: None }
    }
}

impl<M, S> ActorFactoryArgs<(S, EventBus<M>)> for Store<M, S>
where
    M: Model,
    S: CommitStore<M>,
{
    fn create_args((backend, bus): (S, EventBus<M>)) -> Self {
        Store {
            backend,
            bus: Some(bus),
        }
    }
}

impl<M, S> Receive<Commit<M>> for Store<M, S>
where
    M: Model,
    S: CommitStore<M>,
{
    type Msg = StoreMsg<M>;
    fn receive(&mut self, cx: &Context<Self::Msg>, c: Commit<M>, _sender: Sender) {
        trace!("storing {:?}", c);
        let store = self.backend.clone();
        let id = c.entity_id();
        let bus = self.bus.clone();
        let topic_name = format!("{}-events", cx.myself().name());
        let event = c.event.clone();
        cx.system.exec.spawn_ok(async move {
            store.commit(c).await.expect("commit message");
            if bus.is_some() {
                bus.as_ref().unwrap().tell(
                    Publish {
                        topic: topic_name.into(),
                        msg: event,
                    },
                    None,
                );
            }
            debug!("saved commit for {}", id);
        });
    }
}

impl<M, S> Receive<(EntityId, DateTime<Utc>)> for Store<M, S>
where
    M: Model,
    S: CommitStore<M>,
{
    type Msg = StoreMsg<M>;

    fn receive(
        &mut self,
        cx: &Context<Self::Msg>,
        (id, until): (EntityId, DateTime<Utc>),
        sender: Sender,
    ) {
        let store = self.backend.clone();
        cx.system.exec.spawn_ok(async move {
            let snapshot = store.snapshot(id, until).await;
            if snapshot.is_ok() {
                debug!("Loaded snapshot for {}", id);
            } else {
                debug!("Couldn't load {}", id);
            }
            sender
                .unwrap()
                .try_tell(snapshot.ok(), None)
                .expect("can receive snapshot");
        });
    }
}

// list of entities
impl<M, S> Receive<DateTime<Utc>> for Store<M, S>
where
    M: Model,
    S: CommitStore<M>,
{
    type Msg = StoreMsg<M>;

    fn receive(&mut self, cx: &Context<Self::Msg>, until: DateTime<Utc>, sender: Sender) {
        let backend = self.backend.clone();
        let _ = cx.system.exec.spawn_ok(async move {
            let entities = backend
                .clone()
                .entities()
                .and_then(|entity| entity.travel_to(until))
                .try_collect::<Vec<M>>()
                .await
                .expect("list entities");
            sender
                .unwrap()
                .try_tell(entities, None)
                .expect("receive snapshot list");
            debug!("loaded list of snapshots until {}", until);
        });
    }
}

impl<M, S> Receive<EntityId> for Store<M, S>
where
    M: Model,
    S: CommitStore<M>,
{
    type Msg = StoreMsg<M>;

    fn receive(&mut self, _cx: &Context<Self::Msg>, _id: EntityId, _sender: Sender) {
        todo!();
    }
}

#[derive(Debug, Clone)]
pub enum StoreMsg<T: Model> {
    Commit(Commit<T>),
    Snapshot((EntityId, DateTime<Utc>)),
    SnapshotList(DateTime<Utc>),
    Subscribe(EntityId),
}
impl<T: Model> From<Event<T>> for StoreMsg<T> {
    fn from(msg: Event<T>) -> Self {
        StoreMsg::Commit(msg.into())
    }
}
impl<T: Model> From<DateTime<Utc>> for StoreMsg<T> {
    fn from(range: DateTime<Utc>) -> Self {
        StoreMsg::SnapshotList(range)
    }
}
impl<T: Model> From<Commit<T>> for StoreMsg<T> {
    fn from(msg: Commit<T>) -> Self {
        StoreMsg::Commit(msg)
    }
}
impl<T: Model> From<EntityId> for StoreMsg<T> {
    fn from(id: EntityId) -> Self {
        StoreMsg::Subscribe(id)
    }
}
impl<T: Model> From<(EntityId, DateTime<Utc>)> for StoreMsg<T> {
    fn from(snap: (EntityId, DateTime<Utc>)) -> Self {
        StoreMsg::Snapshot(snap)
    }
}

type Author = Option<String>;
type Reason = Option<String>;

/// Commit represents a unique inmutable change to the system made by someone at a specific time
#[derive(Debug, Clone)]
pub struct Commit<T: Model> {
    event: Event<T>,
    when: DateTime<Utc>,
    who: Author,
    why: Reason,
}
impl<T: Model> Commit<T> {
    pub fn new(event: Event<T>, who: Author, why: Reason) -> Self {
        Commit {
            event,
            when: Utc::now(),
            who,
            why,
        }
    }
}

impl<T: Model> Deref for Commit<T> {
    type Target = Event<T>;

    fn deref(&self) -> &Self::Target {
        &self.event
    }
}

impl<T: Model> From<Event<T>> for Commit<T> {
    fn from(e: Event<T>) -> Self {
        Commit::new(e, None, None)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use futures::executor::block_on;
    use riker_patterns::ask::ask;

    #[derive(Default, Clone, Debug)]
    pub struct TestCount {
        id: EntityId,
        pub count: i16,
    }
    impl TestCount {
        pub fn new(c: i16) -> Self {
            Self {
                count: c,
                id: EntityId::new(),
            }
        }
    }
    #[derive(Clone, Debug)]
    pub enum Op {
        Add(i16),
        Sub(i16),
    }
    impl Model for TestCount {
        type Change = Op;
        fn id(&self) -> EntityId {
            self.id
        }
        fn apply_change(&mut self, change: &Self::Change) {
            match change {
                Op::Add(n) => self.count += n,
                Op::Sub(n) => self.count -= n,
            };
        }
    }

    #[test]
    fn load_snapshot() {
        let sys = ActorSystem::new().unwrap();
        let store = sys
            .actor_of_args::<Store<TestCount, _>, _>("test-counts", MemStore::new())
            .unwrap();

        let test = TestCount::default();
        let id = test.id();
        store.tell(Event::Create(test), None);
        store.tell(Event::Change(id, Op::Add(15)), None);
        store.tell(Event::Change(id, Op::Add(5)), None);
        store.tell(Event::Change(id, Op::Sub(9)), None);
        store.tell(Event::Change(id, Op::Add(31)), None);

        let result: Option<TestCount> = block_on(ask(&sys, &store, (id, Utc::now())));
        assert_eq!(result.unwrap().count, 42);
    }

    #[test]
    fn non_existing_entity() {
        let sys = ActorSystem::new().unwrap();
        let store = sys
            .actor_of_args::<Store<TestCount, _>, _>("test-counts", MemStore::new())
            .unwrap();

        let result: Option<TestCount> = block_on(ask(&sys, &store, ("123".into(), Utc::now())));
        assert!(result.is_none());
    }

    #[test]
    fn load_list_of_snapshots() {
        let sys = ActorSystem::new().unwrap();
        let store = sys
            .actor_of_args::<Store<TestCount, _>, _>("test-counts", MemStore::new())
            .unwrap();

        let some_counter = TestCount {
            id: "123".into(),
            count: 42,
        };
        store.tell(Event::Create(some_counter), None);
        store.tell(Event::Create(TestCount::default()), None);
        store.tell(Event::Create(TestCount::default()), None);
        store.tell(Event::Change("123".into(), Op::Add(8)), None);

        let result: Vec<TestCount> = block_on(ask(&sys, &store, Utc::now()));
        assert_eq!(result.len(), 3);
        let some_counter_snapshot = result.iter().find(|s| s.id() == "123".into()).unwrap();
        assert_eq!(some_counter_snapshot.count, 50);
    }

    #[test]
    fn broadcast_event() {
        let sys = ActorSystem::new().unwrap();
        let bus: EventBus<_> = channel("bus", &sys).unwrap();
        let store_name = "test-counts";
        let store = sys
            .actor_of_args::<Store<TestCount, _>, _>(store_name, (MemStore::new(), bus.clone()))
            .unwrap();

        #[derive(Clone, Debug)]
        enum TestSubMsg {
            Event(Event<TestCount>),
            Get,
        }
        impl From<Event<TestCount>> for TestSubMsg {
            fn from(event: Event<TestCount>) -> Self {
                TestSubMsg::Event(event)
            }
        }

        #[derive(Default)]
        struct TestSub(Option<Event<TestCount>>);
        impl Actor for TestSub {
            type Msg = TestSubMsg;
            fn recv(&mut self, _cx: &Context<Self::Msg>, msg: Self::Msg, sender: Sender) {
                match msg {
                    TestSubMsg::Get => {
                        sender.unwrap().try_tell(self.0.clone(), None).unwrap();
                    }
                    TestSubMsg::Event(e) => {
                        self.0 = Some(e);
                    }
                }
            }
        }

        let sub = sys.actor_of::<TestSub>("subscriber").unwrap();
        bus.tell(
            Subscribe {
                topic: format!("{}-events", store_name).into(),
                actor: Box::new(sub.clone()),
            },
            None,
        );

        store.tell(Event::Create(TestCount::default()), None);

        let result: Option<Event<TestCount>> = block_on(ask(&sys, &sub, TestSubMsg::Get));

        assert!(result.is_some());
    }
}
