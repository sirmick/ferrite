// Production transport for the event bus: MQTT.js over the same-origin
// `/mqtt` WebSocket (vite proxies it to ferrited in dev; ferrited
// reverse-proxies it to the embedded broker in both). MQTT 3.1.1 —
// the broker's WebSocket listener speaks v4 only.

import mqtt from 'mqtt';

import { wsUrlFor } from '$lib/api/errors';
import { logs } from '$lib/logs/store.svelte';

import { eventBus, type MqttLike } from './bus';

export function connectEventBus(): () => void {
  const url = wsUrlFor('/mqtt');
  const client = mqtt.connect(url, {
    protocolVersion: 4,
    clientId: `ferrite-web-${Math.random().toString(36).slice(2, 10)}`,
    clean: true,
    keepalive: 30,
    reconnectPeriod: 1000,
    connectTimeout: 10_000,
  });
  client.on('error', (err) => {
    // MQTT.js reconnects on its own; this is diagnostic only. Never
    // `console.error` here — the console is patched into `logs`, and a
    // broker outage would then log once per retry.
    logs.push('client', 'warn', `event bus: ${err.message}`);
  });
  eventBus.attach(client as unknown as MqttLike);
  return () => eventBus.detach();
}
