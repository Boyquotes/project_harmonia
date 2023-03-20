use std::fmt::Debug;

use bevy::{
    ecs::{entity::MapEntities, event::Event},
    prelude::*,
};
use bevy_renet::renet::{RenetClient, RenetServer};
use serde::{de::DeserializeOwned, Serialize};
use tap::TapFallible;

use super::{EventChannel, NetworkEventCounter};
use crate::core::{
    game_world::WorldState,
    network::{
        replication::map_entity::NetworkEntityMap, server::SERVER_ID, sets::NetworkSet,
        REPLICATION_CHANNEL_ID,
    },
};

/// An extension trait for [`App`] for creating client events.
pub(crate) trait ClientEventAppExt {
    /// Registers [`ClientEvent<T>`] event that will be emitted on server after sending `T` event on client.
    fn add_client_event<T: Event + Serialize + DeserializeOwned + Debug>(&mut self) -> &mut Self;

    /// Same as [`add_client_event`], but additionally maps client entities to server before sending.
    fn add_mapped_client_event<T: Event + Serialize + DeserializeOwned + Debug + MapEntities>(
        &mut self,
    ) -> &mut Self;

    /// Same as [`add_client_event`], but uses the specified sending system.
    fn add_client_event_with<T: Event + Serialize + DeserializeOwned + Debug, Marker>(
        &mut self,
        sending_system: impl IntoSystemConfig<Marker>,
    ) -> &mut Self;
}

impl ClientEventAppExt for App {
    fn add_client_event<T: Event + Serialize + DeserializeOwned + Debug>(&mut self) -> &mut Self {
        self.add_client_event_with::<T, _>(sending_system::<T>)
    }

    fn add_mapped_client_event<T: Event + Serialize + DeserializeOwned + Debug + MapEntities>(
        &mut self,
    ) -> &mut Self {
        self.add_client_event_with::<T, _>(mapping_and_sending_system::<T>)
    }

    fn add_client_event_with<T: Event + Serialize + DeserializeOwned + Debug, Marker>(
        &mut self,
        sending_system: impl IntoSystemConfig<Marker>,
    ) -> &mut Self {
        let mut event_counter = self
            .world
            .get_resource_or_insert_with(NetworkEventCounter::default);
        event_counter.client += 1;
        let current_channel_id = REPLICATION_CHANNEL_ID + event_counter.client;

        self.add_event::<T>()
            .add_event::<ClientEvent<T>>()
            .insert_resource(EventChannel::<T>::new(current_channel_id))
            .add_system(sending_system.in_set(NetworkSet::ClientConnected))
            .add_system(
                local_resending_system::<T>
                    .in_set(OnUpdate(WorldState::InWorld))
                    .in_set(NetworkSet::Authoritve),
            )
            .add_system(receiving_system::<T>.in_set(NetworkSet::Server));

        self
    }
}

fn sending_system<T: Event + Serialize + Debug>(
    mut events: EventReader<T>,
    mut client: ResMut<RenetClient>,
    channel: Res<EventChannel<T>>,
) {
    for event in events.iter() {
        let message = bincode::serialize(&event).expect("unable to serialize client event");
        client.send_message(channel.id, message);
        debug!("sent client event {event:?}");
    }
}

fn mapping_and_sending_system<T: Event + MapEntities + Serialize + Debug>(
    mut events: ResMut<Events<T>>,
    mut client: ResMut<RenetClient>,
    entity_map: Res<NetworkEntityMap>,
    channel: Res<EventChannel<T>>,
) {
    for mut event in events.drain() {
        event
            .map_entities(entity_map.to_server())
            .unwrap_or_else(|e| panic!("unable to map entities for client event {event:?}: {e}"));
        let message = bincode::serialize(&event).expect("unable to serialize mapped client event");
        client.send_message(channel.id, message);
        debug!("sent mapped client event {event:?}");
    }
}

/// Transforms [`T`] events into [`ClientEvent<T>`] events to "emulate"
/// message sending for offline mode or when server is also a player
fn local_resending_system<T: Event + Debug>(
    mut events: ResMut<Events<T>>,
    mut client_events: EventWriter<ClientEvent<T>>,
) {
    for event in events.drain() {
        debug!("converted client event {event:?} into a local");
        client_events.send(ClientEvent {
            client_id: SERVER_ID,
            event,
        })
    }
}

fn receiving_system<T: Event + DeserializeOwned + Debug>(
    mut client_events: EventWriter<ClientEvent<T>>,
    mut server: ResMut<RenetServer>,
    channel: Res<EventChannel<T>>,
) {
    for client_id in server.clients_id() {
        while let Some(message) = server.receive_message(client_id, channel.id) {
            if let Ok(event) = bincode::deserialize(&message)
                .tap_err(|e| error!("unable to deserialize event from client {client_id}: {e}"))
            {
                debug!("received event {event:?} from client {client_id}");
                client_events.send(ClientEvent { client_id, event });
            }
        }
    }
}

/// An event indicating that a message from client was received.
/// Emited only on server.
#[derive(Clone, Copy)]
pub(crate) struct ClientEvent<T> {
    pub(crate) client_id: u64,
    pub(crate) event: T,
}

#[cfg(test)]
mod tests {
    use bevy::ecs::{
        entity::{EntityMap, MapEntitiesError},
        event::Events,
    };
    use serde::Deserialize;

    use super::*;
    use crate::core::network::{network_preset::NetworkPresetPlugin, sets::NetworkSetsPlugin};

    #[test]
    fn sending_receiving() {
        let mut app = App::new();
        app.add_plugin(NetworkSetsPlugin)
            .add_client_event::<DummyEvent>()
            .add_plugin(NetworkPresetPlugin::client_and_server());

        let mut dummy_events = app.world.resource_mut::<Events<DummyEvent>>();
        dummy_events.send(DummyEvent);

        app.update();
        app.update();

        let client_events = app.world.resource::<Events<ClientEvent<DummyEvent>>>();
        assert_eq!(client_events.len(), 1);
    }

    #[test]
    fn mapping() {
        let mut app = App::new();
        app.add_plugin(NetworkSetsPlugin)
            .init_resource::<NetworkEntityMap>()
            .add_mapped_client_event::<MappedEvent>()
            .add_plugin(NetworkPresetPlugin::client_and_server());

        let client_entity = Entity::from_raw(0);
        let server_entity = Entity::from_raw(client_entity.index() + 1);
        app.world
            .resource_mut::<NetworkEntityMap>()
            .insert(server_entity, client_entity);

        let mut mapped_events = app.world.resource_mut::<Events<MappedEvent>>();
        mapped_events.send(MappedEvent(client_entity));

        app.update();
        app.update();

        let mapped_entities: Vec<_> = app
            .world
            .resource_mut::<Events<ClientEvent<MappedEvent>>>()
            .drain()
            .map(|event| event.event.0)
            .collect();
        assert_eq!(mapped_entities, [server_entity]);
    }

    #[test]
    fn local_resending() {
        let mut app = App::new();
        app.add_plugin(NetworkSetsPlugin)
            .add_state::<WorldState>()
            .add_client_event::<DummyEvent>();

        app.world
            .resource_mut::<NextState<WorldState>>()
            .set(WorldState::InWorld);
        let mut dummy_events = app.world.resource_mut::<Events<DummyEvent>>();
        dummy_events.send(DummyEvent);

        app.update();

        assert!(app.world.resource::<Events<DummyEvent>>().is_empty());

        let client_events = app.world.resource::<Events<ClientEvent<DummyEvent>>>();
        assert_eq!(client_events.len(), 1);
    }

    #[derive(Deserialize, Serialize, Debug)]
    struct DummyEvent;

    #[derive(Deserialize, Serialize, Debug)]
    struct MappedEvent(Entity);

    impl MapEntities for MappedEvent {
        fn map_entities(&mut self, entity_map: &EntityMap) -> Result<(), MapEntitiesError> {
            self.0 = entity_map.get(self.0)?;
            Ok(())
        }
    }
}
