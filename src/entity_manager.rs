use crate::{Entity, EntityId, Query, CQRS, ES};
use futures::channel::oneshot::{channel, Sender as ChannelSender};
use riker::actors::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct Manager {
    sys: ActorSystem,
    entities: HashMap<String, BasicActorRef>,
}

impl Manager {
    pub fn new(sys: ActorSystem) -> Self {
        Manager {
            sys,
            entities: HashMap::new(),
        }
    }

    pub fn sys(&self) -> &ActorSystem {
        &self.sys
    }

    pub fn register<T: ES + EntityName>(mut self, args: T::Args) -> Self {
        let entity = self
            .sys
            .actor_of_args::<Entity<T>, _>(T::NAME, args)
            .expect(&format!("create entity {}", T::NAME));
        self.entities.insert(T::NAME.into(), entity.into());
        self
    }

    pub async fn command<C>(&self, cmd: C) -> EntityId
    where
        C: Message + EntityName,
    {
        let entity = self.entity(<C as EntityName>::NAME);
        self.ask(entity, CQRS::Cmd(cmd)).await
    }

    pub async fn query<E>(&self, id: EntityId) -> Option<E::Model>
    where
        E: ES + EntityName,
    {
        let entity = self.entity(<E as EntityName>::NAME);
        let q: CQRS<E::Cmd> = CQRS::Query(Query::One(id.into()));
        self.ask(entity, q).await
    }

    pub fn entity(&self, name: &str) -> BasicActorRef {
        self.entities.get(name).unwrap().clone()
    }

    async fn ask<Msg: Message, R: Message>(&self, entity: BasicActorRef, msg: Msg) -> R {
        let (tx, rx) = channel::<R>();
        let tx = Arc::new(Mutex::new(Some(tx)));
        let tmp_sender = self.sys.tmp_actor_of_args::<AskActor<R>, _>(tx).unwrap();

        entity.try_tell(msg, tmp_sender).expect("can send message");
        rx.await.unwrap()
    }
}

// NOTE: work around to get entity name for commands
// TODO derive from implementor struct name
pub trait EntityName {
    const NAME: &'static str;
}

struct AskActor<Msg> {
    tx: Arc<Mutex<Option<ChannelSender<Msg>>>>,
}

impl<Msg: Message> ActorFactoryArgs<Arc<Mutex<Option<ChannelSender<Msg>>>>> for AskActor<Msg> {
    fn create_args(tx: Arc<Mutex<Option<ChannelSender<Msg>>>>) -> Self {
        AskActor { tx }
    }
}

impl<Msg: Message> Actor for AskActor<Msg> {
    type Msg = Msg;

    fn recv(&mut self, ctx: &Context<Self::Msg>, msg: Self::Msg, _: Sender) {
        if let Ok(mut tx) = self.tx.lock() {
            tx.take().unwrap().send(msg).unwrap();
        }
        ctx.stop(&ctx.myself);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Event;
    use async_trait::async_trait;
    use futures::executor::block_on;

    #[derive(Debug)]
    struct Entity1;
    impl EntityName for Entity1 {
        const NAME: &'static str = "entity-1";
    }
    impl EntityName for () {
        const NAME: &'static str = "entity-1";
    }
    #[async_trait]
    impl ES for Entity1 {
        type Args = ();
        type Model = ();
        type Cmd = ();
        type Error = ();
        fn new(_cx: &Context<CQRS<Self::Cmd>>, _args: Self::Args) -> Self {
            Entity1
        }
        async fn handle_command(
            &mut self,
            _cmd: Self::Cmd,
        ) -> Result<crate::Commit<Self::Model>, Self::Error> {
            Ok(Event::Create(()).into())
        }
    }

    #[test]
    fn register_entities() {
        let mgr = Manager::new(ActorSystem::new().unwrap()).register::<Entity1>(());
        let id = block_on(mgr.command(()));
        assert_eq!(id, "dummy".into());
    }
}
