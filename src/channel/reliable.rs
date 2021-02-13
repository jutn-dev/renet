use crate::channel::{Channel, ChannelConfig, Message, MessageSend, PacketSent};
use crate::sequence_buffer::SequenceBuffer;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct ReliableOrderedChannelConfig {
    pub sent_packet_buffer_size: usize,
    pub message_send_queue_size: usize,
    pub message_receive_queue_size: usize,
    pub max_message_per_packet: u32,
    pub packet_budget_bytes: Option<u32>,
    pub message_resend_time: Duration,
}

impl Default for ReliableOrderedChannelConfig {
    fn default() -> Self {
        Self {
            sent_packet_buffer_size: 1024,
            message_send_queue_size: 1024,
            message_receive_queue_size: 1024,
            max_message_per_packet: 256,
            packet_budget_bytes: None,
            message_resend_time: Duration::from_millis(100),
        }
    }
}

impl ChannelConfig for ReliableOrderedChannelConfig {
    fn new_channel(&self, current_time: Instant) -> Box<dyn Channel> {
        Box::new(ReliableOrderedChannel::new(current_time, self.clone()))
    }
}

pub struct ReliableOrderedChannel {
    config: ReliableOrderedChannelConfig,
    packets_sent: SequenceBuffer<PacketSent>,
    messages_send: SequenceBuffer<MessageSend>,
    messages_received: SequenceBuffer<Message>,
    send_message_id: u16,
    received_message_id: u16,
    num_messages_sent: u64,
    num_messages_received: u64,
    oldest_unacked_message_id: u16,
    current_time: Instant,
}

impl ReliableOrderedChannel {
    pub fn new(current_time: Instant, config: ReliableOrderedChannelConfig) -> Self {
        Self {
            current_time,
            packets_sent: SequenceBuffer::with_capacity(config.sent_packet_buffer_size),
            messages_send: SequenceBuffer::with_capacity(config.message_send_queue_size),
            messages_received: SequenceBuffer::with_capacity(config.message_receive_queue_size),
            send_message_id: 0,
            received_message_id: 0,
            num_messages_received: 0,
            num_messages_sent: 0,
            oldest_unacked_message_id: 0,
            config,
        }
    }

    pub fn has_messages_to_send(&self) -> bool {
        self.oldest_unacked_message_id != self.send_message_id
    }

    // TODO: use bits or bytes?
    fn get_messages_id_to_send(&mut self, available_bits: Option<u32>) -> Option<Vec<u16>> {
        if !self.has_messages_to_send() {
            return None;
        }

        // TODO: Should we even be doing this?
        let available_bits = available_bits.unwrap_or(u32::MAX);

        let mut available_bits = if let Some(packet_budget) = self.config.packet_budget_bytes {
            std::cmp::min(packet_budget * 8, available_bits)
        } else {
            available_bits
        };

        let message_limit = std::cmp::min(
            self.config.message_send_queue_size,
            self.config.message_receive_queue_size,
        );
        let mut num_messages = 0;
        let mut messages_id = vec![];

        for i in 0..message_limit {
            if num_messages == self.config.max_message_per_packet {
                break;
            }
            let message_id = self.oldest_unacked_message_id + i as u16;
            let message_send = self.messages_send.get_mut(message_id);
            if let Some(message_send) = message_send {
                let send = if let Some(last_time_sent) = message_send.last_time_sent {
                    (last_time_sent + self.config.message_resend_time) <= self.current_time
                } else {
                    true
                };

                if send && message_send.serialized_size_bits <= available_bits {
                    messages_id.push(message_id);
                    num_messages += 1;
                    available_bits -= message_send.serialized_size_bits;
                }
            }
        }

        if !messages_id.is_empty() {
            return Some(messages_id);
        }
        None
    }

    fn add_messages_packet_entry(&mut self, messages_id: Vec<u16>, sequence: u16) {
        let packet_sent = PacketSent::new(messages_id);
        self.packets_sent.insert(sequence, packet_sent);
    }

    fn update_oldest_message_ack(&mut self) {
        let stop_id = self.messages_send.sequence();

        while self.oldest_unacked_message_id != stop_id
            && !self.messages_send.exists(self.oldest_unacked_message_id)
        {
            self.oldest_unacked_message_id += 1;
        }
    }
}

impl Channel for ReliableOrderedChannel {
    fn update_current_time(&mut self, time: Instant) {
        self.current_time = time;
    }

    fn get_messages_to_send(
        &mut self,
        available_bits: Option<u32>,
        sequence: u16,
    ) -> Option<Vec<Message>> {
        if let Some(messages_id) = self.get_messages_id_to_send(available_bits) {
            let messages: Vec<Message> = messages_id
                .iter()
                .map(|m_id| {
                    let message_send = self.messages_send.get_mut(*m_id).unwrap();
                    message_send.last_time_sent = Some(self.current_time);
                    message_send.message.clone()
                })
                .collect();

            self.add_messages_packet_entry(messages_id, sequence);
            return Some(messages);
        }
        None
    }

    fn process_messages(&mut self, mut messages: Vec<Message>) {
        for message in messages.drain(..) {
            // TODO: validate min max message_id based on config queue size
            if !self.messages_received.exists(message.id) {
                self.messages_received.insert(message.id, message);
            }
        }
    }

    fn process_ack(&mut self, ack: u16) {
        if let Some(sent_packet) = self.packets_sent.get_mut(ack) {
            if sent_packet.acked {
                return;
            }
            sent_packet.acked = true;

            for &message_id in sent_packet.messages_id.iter() {
                if self.messages_send.exists(message_id) {
                    self.messages_send.remove(message_id);
                }
            }
            self.update_oldest_message_ack();
        }
    }

    fn send_message(&mut self, message_payload: Box<[u8]>) {
        // assert that can send message?
        // Check config for max num size
        let message_id = self.send_message_id;
        self.send_message_id = self.send_message_id.wrapping_add(1);

        let entry = MessageSend::new(Message::new(message_id, message_payload));
        self.messages_send.insert(message_id, entry);

        self.num_messages_sent += 1;
    }

    fn receive_message(&mut self) -> Option<Box<[u8]>> {
        let received_message_id = self.received_message_id;

        if !self.messages_received.exists(received_message_id) {
            return None;
        }

        self.received_message_id = self.received_message_id.wrapping_add(1);
        self.num_messages_received += 1;

        if let Some(message) = self.messages_received.remove(received_message_id) {
            return Some(message.payload);
        }
        None
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::time::Duration;

    #[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
    enum TestMessages {
        Noop,
        First,
        Second(u32),
        Third(u64),
    }

    impl Default for TestMessages {
        fn default() -> Self {
            return TestMessages::Noop;
        }
    }

    impl TestMessages {
        fn serialize(&self) -> Box<[u8]> {
            bincode::serialize(&self).unwrap().into_boxed_slice()
        }
    }

    #[test]
    fn send_message() {
        let config = ReliableOrderedChannelConfig::default();
        let mut channel: ReliableOrderedChannel =
            ReliableOrderedChannel::new(Instant::now(), config);
        let sequence = 0;

        assert!(!channel.has_messages_to_send());
        assert_eq!(channel.num_messages_sent, 0);

        channel.send_message(TestMessages::Second(0).serialize());
        assert_eq!(channel.num_messages_sent, 1);
        assert!(channel.receive_message().is_none());

        let messages = channel.get_messages_to_send(None, sequence).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].payload, TestMessages::Second(0).serialize());

        assert!(channel.has_messages_to_send());

        channel.process_ack(sequence);
        assert!(!channel.has_messages_to_send());
    }

    #[test]
    fn receive_message() {
        let config = ReliableOrderedChannelConfig::default();
        let mut channel: ReliableOrderedChannel =
            ReliableOrderedChannel::new(Instant::now(), config);

        let messages = vec![
            Message::new(0, TestMessages::First.serialize()),
            Message::new(1, TestMessages::Second(0).serialize()),
        ];

        channel.process_messages(messages);

        let message = channel.receive_message().unwrap();
        assert_eq!(message, TestMessages::First.serialize());

        let message = channel.receive_message().unwrap();
        assert_eq!(message, TestMessages::Second(0).serialize());

        assert_eq!(channel.num_messages_received, 2);
    }

    #[test]
    fn over_budget() {
        let first_message = TestMessages::Third(0);
        let second_message = TestMessages::Third(1);

        let message = Message::new(0, first_message.serialize());

        let mut config = ReliableOrderedChannelConfig::default();
        config.packet_budget_bytes = Some(bincode::serialized_size(&message).unwrap() as u32);
        let mut channel: ReliableOrderedChannel =
            ReliableOrderedChannel::new(Instant::now(), config);
        let sequence = 0;

        channel.send_message(first_message.serialize());
        channel.send_message(second_message.serialize());

        let messages = channel.get_messages_to_send(None, sequence);
        assert!(messages.is_some());
        let messages = messages.unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].payload, first_message.serialize());

        channel.process_ack(0);

        let messages = channel.get_messages_to_send(None, sequence + 1);
        assert!(messages.is_some());
        let messages = messages.unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].payload, second_message.serialize());
    }

    #[test]
    fn resend_message() {
        let mut config = ReliableOrderedChannelConfig::default();
        let resend_time = 200;
        config.message_resend_time = Duration::from_millis(resend_time);
        let now = Instant::now();
        let mut channel: ReliableOrderedChannel = ReliableOrderedChannel::new(now, config);
        let mut sequence = 0;

        channel.send_message(TestMessages::First.serialize());

        let messages = channel.get_messages_to_send(None, sequence).unwrap();
        sequence += 1;

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].payload, TestMessages::First.serialize());
        assert_eq!(messages[0].id, 0);

        let messages = channel.get_messages_to_send(None, sequence);
        sequence += 1;

        assert!(messages.is_none());

        channel.update_current_time(now + Duration::from_millis(resend_time));

        let messages = channel.get_messages_to_send(None, sequence).unwrap();

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].payload, TestMessages::First.serialize());
        assert_eq!(messages[0].id, 0);
    }
}