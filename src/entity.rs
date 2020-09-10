use crate::store::{Commit, CommitStore, Store, StoreRef};
use crate::EntityId;
use async_trait::async_trait;
use chrono::prelude::*;
use futures::lock::Mutex;
use riker::actors::*;
use std::fmt;
use std::sync::Arc;

/// An Aggregate is the projected data of a series of events of an entity,
/// given an initial state update events are applied to it until it reaches the desired state.
pub trait Model: Message {
    type Change: Message;
    fn id(&self) -> EntityId;
    fn apply_change(&mut self, change: &Self::Change);
}

pub type Result<E> = std::result::Result<Commit<<E as ES>::Model>, <E as ES>::Error>;

/// Implement this trait to allow your entity handle external commands
#[async_trait]
pub trait ES: EntityName + fmt::Debug + Send + Sync + 'static {
    type Args: ActorArgs;
    type Model: Model;
    type Cmd: Message;
    type Error: fmt::Debug;

    /// The entity constructor receives a Riker context to be able to interact
    /// with other actors.
    fn new(cx: &Context<CQRS<Self::Cmd>>, args: Self::Args) -> Self;

    async fn handle_command(&mut self, _cmd: Self::Cmd) -> Result<Self>;
}

/// Entity is an actor that handles user commands running the buissiness logic defined
/// in the handler callback and "commit" changes of the model to the configured
/// event store. Will also use the store to query a stored entity data applying any
/// change that has been recorded up to the specified moment in time.
pub struct Entity<E: ES, S: CommitStore<E::Model>> {
    store: Option<StoreRef<E::Model>>,
    store_backend: Option<S>,
    args: E::Args,
    es: Option<Arc<Mutex<E>>>,
}

impl<E, S, Args> ActorFactoryArgs<(S, Args)> for Entity<E, S>
where
    Args: ActorArgs,
    E: ES<Args = Args>,
    S: CommitStore<E::Model>,
{
    fn create_args((store_backend, args): (S, Args)) -> Self {
        Entity {
            store: None,
            store_backend: Some(store_backend),
            es: None,
            args,
        }
    }
}

impl<E, S> Actor for Entity<E, S>
where
    E: ES,
    S: CommitStore<E::Model>,
{
    type Msg = CQRS<E::Cmd>;

    fn pre_start(&mut self, ctx: &Context<Self::Msg>) {
        let entity_handler = Arc::new(Mutex::new(E::new(ctx, self.args.clone())));
        let store_backend = self.store_backend.take().unwrap();
        self.es = Some(entity_handler);
        self.store = Some(
            ctx.actor_of_args::<Store<E::Model, S>, _>(
                &format!("{}_store", E::NAME),
                store_backend,
            )
            .unwrap(),
        );
    }

    fn recv(&mut self, ctx: &Context<Self::Msg>, msg: Self::Msg, sender: Sender) {
        match msg {
            CQRS::Query(q) => self.receive(ctx, q, sender),
            CQRS::Cmd(cmd) => {
                let store = self.store.as_ref().unwrap().clone();
                let es = self.es.clone();
                ctx.system.exec.spawn_ok(async move {
                    let cmd_dbg = format!("{:?}", cmd);
                    debug!("processing command {}", cmd_dbg);
                    let commit = es
                        .unwrap()
                        .lock()
                        .await
                        .handle_command(cmd)
                        .await
                        .expect("Failed handling command");
                    let entity_id = commit.entity_id();
                    store.tell(commit, None);

                    if let Some(sender) = sender {
                        let _ = sender
                            .try_tell(entity_id, None)
                            .map_err(|_| warn!("Couldn't signal completion of {}", cmd_dbg));
                    }
                });
            }
        };
    }
}

impl<E, S> Receive<Query> for Entity<E, S>
where
    E: ES,
    S: CommitStore<E::Model>,
{
    type Msg = CQRS<E::Cmd>;
    fn receive(&mut self, _ctx: &Context<Self::Msg>, q: Query, sender: Sender) {
        match q {
            Query::One(id) => self.store.as_ref().unwrap().tell((id, Utc::now()), sender),
            Query::All => self.store.as_ref().unwrap().tell(Utc::now(), sender),
        }
    }
}

#[derive(Clone, Debug)]
pub enum CQRS<C> {
    Cmd(C),
    Query(Query),
}
impl<C> From<Query> for CQRS<C> {
    fn from(q: Query) -> Self {
        CQRS::Query(q)
    }
}

#[derive(Clone, Debug)]
pub enum Query {
    All,
    One(EntityId),
}

// NOTE: work around to get entity name for commands
// TODO derive from implementor struct name
pub trait EntityName {
    const NAME: &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::{Op, TestCount};
    use crate::store::MemStore;
    use crate::{macros::*, Event};
    use futures::executor::block_on;
    use riker_patterns::ask::ask;

    #[derive(EntityName, Debug)]
    struct Test {
        sys: ActorSystem,
        entity: ActorRef<CQRS<<Self as ES>::Cmd>>,
        _foo: String,
    }
    #[async_trait]
    impl ES for Test {
        type Args = (u8, String);
        type Model = TestCount;
        type Cmd = TestCmd;
        type Error = String;

        fn new(cx: &Context<CQRS<Self::Cmd>>, (num, txt): Self::Args) -> Self {
            Test {
                _foo: format!("{}{}", num, txt),
                sys: cx.system.clone(),
                entity: cx.myself(),
            }
        }

        async fn handle_command(&mut self, cmd: Self::Cmd) -> Result<Self> {
            let event = match cmd {
                TestCmd::Create42 => Event::Create(TestCount::new(42)),
                TestCmd::Create99 => Event::Create(TestCount::new(99)),
                TestCmd::Double(id) => {
                    let res: Option<TestCount> = ask(&self.sys, &self.entity, Query::One(id)).await;
                    let res = res.ok_or("Not found")?;
                    Event::Change(res.id(), Op::Add(res.count))
                }
            };
            Ok(event.into())
        }
    }
    #[derive(Clone, Debug)]
    enum TestCmd {
        Create42,
        Create99,
        Double(EntityId),
    }

    #[test]
    fn command_n_query() {
        let sys = ActorSystem::new().unwrap();
        let entity = sys
            .actor_of_args::<Entity<Test, MemStore<_>>, _>(
                "counts",
                (MemStore::new(), (42, "42".into())),
            )
            .unwrap();

        let _: EntityId = block_on(ask(&sys, &entity, CQRS::Cmd(TestCmd::Create42)));
        let _: EntityId = block_on(ask(&sys, &entity, CQRS::Cmd(TestCmd::Create99)));
        let counts: Vec<TestCount> = block_on(ask(&sys, &entity, Query::All));

        assert_eq!(counts.len(), 2);
        let count42 = counts.iter().find(|c| c.count == 42);
        let count99 = counts.iter().find(|c| c.count == 99);
        assert!(count42.is_some());
        assert!(count99.is_some());

        let id = count42.unwrap().id();
        let _: EntityId = block_on(ask(&sys, &entity, CQRS::Cmd(TestCmd::Double(id))));
        let result: Option<TestCount> = block_on(ask(&sys, &entity, Query::One(id)));
        assert_eq!(result.unwrap().count, 84);
    }
}
