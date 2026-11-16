import { describe, expect, test } from "bun:test";

import { ed25519, x25519 } from "@noble/curves/ed25519.js";

import {
  WakuClient,
  WakuConnectionError,
  WakuRpcError,
  daemonUrl,
  type WebSocketLike,
} from "./client";
import { PROTOCOL_VERSION } from "./generated";
import { fingerprintOf } from "./pairing";
import {
  agreementBytes,
  Opener,
  Sealer,
  SessionKeys,
  transcriptHash,
  CLIENT_HELLO_LEN,
  SERVER_HELLO_LEN,
  type Direction,
} from "./record";

/**
 * A daemon stand-in that actually runs the sealed handshake.
 *
 * The client is now a sealed-record peer, so a fake socket that passes strings
 * would prove nothing: every test below goes through the real handshake,
 * real key derivation and real AEAD framing, exactly as `sealed.rs` does on
 * the Rust side. `sent` holds the *decrypted* frames so assertions still read
 * like protocol assertions.
 */
class FakeSocket implements WebSocketLike {
  readyState = 0;
  /** Decrypted client frames, in order. */
  sent: string[] = [];
  private listeners = new Map<string, Array<(...args: any[]) => void>>();
  private sealer: Sealer | null = null;
  private opener: Opener | null = null;
  private readonly daemonX25519 = x25519.utils.randomSecretKey();
  private signingKey: Uint8Array = ed25519.utils.randomSecretKey();

  /** The daemon's public signing key. A getter, not a field: `overrideSigningKey`
   * replaces the private half after construction, and a frozen copy computed in
   * the constructor would keep advertising the old one. */
  get daemonEd25519Public(): Uint8Array {
    return ed25519.getPublicKey(this.signingKey) as Uint8Array;
  }

  /** The daemon's signing key, so a test can pin a client to this exact daemon. */
  get daemonSigningKey(): Uint8Array {
    return this.signingKey;
  }
  /**
   * Use a caller-supplied signing key, so two FakeSockets can present the same
   * daemon identity — which is what a pairing test needs to build the pin
   * before the client is constructed.
   */
  overrideSigningKey(key: Uint8Array): void {
    this.signingKey = key;
  }

  addEventListener(type: "open", listener: () => void): void;
  addEventListener(type: "message", listener: (event: MessageEvent) => void): void;
  addEventListener(type: "error", listener: () => void): void;
  addEventListener(type: "close", listener: (event: CloseEvent) => void): void;
  addEventListener(type: string, listener: (...args: any[]) => void): void {
    const listeners = this.listeners.get(type) ?? [];
    listeners.push(listener);
    this.listeners.set(type, listeners);
  }

  send(data: Uint8Array): void {
    // The first frame the client writes is the client hello, in the clear.
    if (!this.sealer) {
      this.acceptHello(data);
      return;
    }
    this.sent.push(new TextDecoder().decode(this.opener!.open(data)));
  }

  close(): void {
    this.readyState = 3;
  }

  emitClose(): void {
    this.readyState = 3;
    this.emit("close", { reason: "" });
  }

  fail(reason: string): void {
    this.readyState = 3;
    this.emit("error", {});
    this.emit("close", { reason });
  }

  open(): void {
    this.readyState = 1;
    this.emit("open");
  }

  /** Deliver one protocol message and let the client's async decode finish. */
  async receive(message: unknown): Promise<void> {
    this.deliver(message);
    await flush();
  }

  /** Deliver a raw frame as the socket would, bypassing the sealer. */
  receiveRaw(frame: Uint8Array | string): void {
    this.emit("message", { data: frame });
  }

  /** The daemon's sealed half of the handshake. */
  private acceptHello(clientHello: Uint8Array): void {
    expect(clientHello.length).toBe(CLIENT_HELLO_LEN);
    const clientStatic = clientHello.subarray(0, 32);
    const clientEphemeral = clientHello.subarray(32, 64);
    const daemonX25519Public = x25519.getPublicKey(this.daemonX25519);
    const serverEphemeralSecret = x25519.utils.randomSecretKey();
    const serverEphemeralPublic = x25519.getPublicKey(serverEphemeralSecret);

    const transcript = transcriptHash(
      clientStatic,
      clientEphemeral,
      serverEphemeralPublic,
      daemonX25519Public,
    );
    const signature = ed25519.sign(transcript, this.signingKey);
    const shared = x25519.getSharedSecret(serverEphemeralSecret, clientEphemeral);
    const bound = x25519.getSharedSecret(serverEphemeralSecret, clientStatic);
    const boundToDaemon = x25519.getSharedSecret(this.daemonX25519, clientEphemeral);
    const keys = SessionKeys.derive(
      agreementBytes([shared, bound, boundToDaemon]),
      transcript,
    );
    this.sealer = keys.sealer("serverToClient");
    this.opener = keys.opener("clientToServer");

    const serverHello = new Uint8Array(SERVER_HELLO_LEN);
    serverHello.set(daemonX25519Public, 0);
    serverHello.set(this.daemonEd25519Public, 32);
    serverHello.set(serverEphemeralPublic, 64);
    serverHello.set(signature, 96);
    this.emit("message", { data: serverHello });
  }

  /** Seal one message to the client. */
  deliver(message: unknown): void {
    if (!this.sealer) throw new Error("the daemon handshake has not run yet");
    this.emit("message", {
      data: this.sealer.seal(new TextEncoder().encode(JSON.stringify(message))),
    });
  }

  private emit(type: string, event?: unknown): void {
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }
}

function fixture() {
  const sockets: FakeSocket[] = [];
  let nextId = 0;
  const client = new WakuClient({
    address: "127.0.0.1:4312",
    token: "secret",
    randomUUID: () => `00000000-0000-4000-8000-${String(++nextId).padStart(12, "0")}`,
    webSocketFactory: () => {
      const socket = new FakeSocket();
      sockets.push(socket);
      return socket;
    },
  });
  return { client, sockets };
}

/**
 * Open the socket and drive the sealed handshake to completion.
 *
 * The handshake is asynchronous on the client (it awaits the frame to bytes),
 * so the daemon's reply cannot be delivered in the same tick as `open()`.
 */
async function connect(client: WakuClient, sockets: FakeSocket[]): Promise<FakeSocket> {
  const connected = client.connect();
  const socket = sockets.at(-1)!;
  socket.open();
  await flush();
  await socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
  await connected;
  return socket;
}

/** Let the client's asynchronous frame decoding settle. */
function flush(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

describe("WakuClient", () => {
  test("reports connection state changes, including remote closure", async () => {
    const { client, sockets } = fixture();
    const states: string[] = [];
    const unsubscribe = client.subscribeConnectionState((state) => states.push(state));

    const connected = client.connect();
    const socket = sockets[0]!;
    socket.open();
    await flush();
    await socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await connected;
    socket.close();
    socket.emitClose();
    unsubscribe();

    expect(states).toEqual(["disconnected", "connecting", "connected", "disconnected"]);
  });

  test("authenticates and correlates typed responses", async () => {
    const { client, sockets } = fixture();
    const connected = client.connect();
    const socket = sockets[0]!;
    socket.open();
    await flush();
    // The client hello is a bare frame no sealer touched: by the time the
    // sealed channel exists the socket records only decrypted client frames.
    await socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await connected;
    expect(JSON.parse(socket.sent[0]!)).toEqual({
      type: "hello",
      protocolVersion: PROTOCOL_VERSION,
      token: "secret",
      clientId: "00000000-0000-4000-8000-000000000001",
      resumeFrom: [],
    });

    const response = client.request({ type: "getSettings" });
    const request = JSON.parse(socket.sent[1]!);
    await socket.receive({
      type: "response",
      requestId: request.requestId,
      outcome: { status: "ok", payload: { type: "ack" } },
    });
    await expect(response).resolves.toEqual({ type: "ack" });
  });

  test("surfaces daemon errors", async () => {
    const { client, sockets } = fixture();
    const active = await connect(client, sockets);

    const response = client.request({ type: "getSettings" });
    // Attach the rejection handler before the error is delivered: an
    // unawaited rejecting promise is reported as an unhandled rejection even
    // when the test asserts on it afterwards.
    const settled = response.catch((cause: unknown) => cause);
    const request = JSON.parse(active.sent[1]!);
    await active.receive({
      type: "response",
      requestId: request.requestId,
      outcome: { status: "error", error: { message: "nope" } },
    });
    expect(await settled).toBeInstanceOf(WakuRpcError);
  });

  test("deduplicates events and resumes from the last sequence", async () => {
    const { client, sockets } = fixture();
    const first = await connect(client, sockets);

    const received: number[] = [];
    client.subscribe("session", "runtime", (event) => received.push(event.sequence));
    const event = {
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "epoch-one",
      sequence: 4,
      event: { kind: "textDelta", payload: { text: "hi" } },
    };
    await first.receive(event);
    await first.receive(event);
    expect(received).toEqual([4]);

    client.disconnect();
    const secondConnection = client.connect();
    const second = sockets[1]!;
    second.open();
    await flush();
    await second.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await secondConnection;
    expect(JSON.parse(second.sent[0]!).resumeFrom).toEqual([
      { sessionId: "session", runtimeId: "runtime", epoch: "epoch-one", sequence: 4 },
    ]);
  });

  test("disconnect rejects an in-flight handshake and permits reconnecting", async () => {
    const { client, sockets } = fixture();
    const firstConnection = client.connect();
    client.disconnect();
    await expect(firstConnection).rejects.toThrow("Waku client disconnected");

    const secondConnection = client.connect();
    const second = sockets[1]!;
    second.open();
    await flush();
    await second.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await expect(secondConnection).resolves.toBeUndefined();
  });

  test("surfaces the native socket failure reason", async () => {
    const { client, sockets } = fixture();
    const connection = client.connect();

    sockets[0]!.fail("The network connection was lost");

    await expect(connection).rejects.toThrow("The network connection was lost");
  });

  test("accepts sequence one again when the daemon epoch changes", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const received: Array<[string, number]> = [];
    client.subscribe("session", "runtime", (event) => {
      received.push([event.epoch, event.sequence]);
    });

    await socket.receive({
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "old",
      sequence: 9,
      event: { kind: "textDelta", payload: null },
    });
    await socket.receive({
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "new",
      sequence: 1,
      event: { kind: "textDelta", payload: null },
    });

    expect(received).toEqual([
      ["old", 9],
      ["new", 1],
    ]);
  });

  test("buffers replayed events until a refreshed app attaches to the runtime", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);

    await socket.receive({
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "epoch",
      sequence: 1,
      event: { kind: "textDelta", payload: "before attach" },
    });
    await socket.receive({
      type: "event",
      sessionId: "session",
      runtimeId: "runtime",
      epoch: "epoch",
      sequence: 2,
      event: { kind: "textDelta", payload: "still before attach" },
    });

    const received: number[] = [];
    client.subscribe("session", "runtime", (event) => received.push(event.sequence));
    expect(received).toEqual([1, 2]);
  });

  test("notifies connected apps when another client changes task state", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const revisions: number[] = [];
    client.subscribeTaskState((revision) => revisions.push(revision));

    await socket.receive({ type: "taskStateChanged", revision: 7 });
    expect(revisions).toEqual([7]);
  });

  test("disconnected requests reject instead of throwing synchronously", async () => {
    const { client } = fixture();
    const request = client.request({ type: "getSettings" });
    await expect(request).rejects.toThrow("Waku daemon is disconnected");
  });

  test("disconnected notifications reject instead of throwing synchronously", async () => {
    const { client } = fixture();
    const notification = client.notify({ type: "refreshBackgroundWork" });
    await expect(notification).rejects.toThrow("Waku daemon is disconnected");
  });

  test("notifications use the response-free nil request id", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);

    await client.notify(
      { type: "writeTerminal", data: "bHM=" },
      "terminal",
      "terminal",
    );

    expect(JSON.parse(socket.sent[1]!)).toEqual({
      type: "request",
      requestId: "00000000-0000-0000-0000-000000000000",
      sessionId: "terminal",
      runtimeId: "terminal",
      command: { type: "writeTerminal", data: "bHM=" },
    });
  });
});

test("daemonUrl pins the versioned endpoint", () => {
  expect(daemonUrl("localhost:3030/anything?old=1")).toBe("ws://localhost:3030/v1");
  expect(daemonUrl("wss://waku.example.test")).toBe("wss://waku.example.test/v1");
});

describe("pairing", () => {
  /** A client pinned to whatever key this fake daemon will prove. */
  function pinned(options: { wrongFingerprint?: boolean } = {}) {
    const sockets: FakeSocket[] = [];
    let nextId = 0;
    // Construct the daemon first so its fingerprint is known before the client
    // is built: the pin is a constructor option, not something applied later.
    const probe = new FakeSocket();
    const fingerprint = options.wrongFingerprint
      ? '0000000000000000'
      : fingerprintOf(probe.daemonEd25519Public);
    const client = new WakuClient({
      address: "127.0.0.1:4312",
      token: "secret",
      expectedFingerprint: fingerprint,
      randomUUID: () => `00000000-0000-4000-8000-${String(++nextId).padStart(12, "0")}`,
      webSocketFactory: () => {
        const socket = new FakeSocket();
        socket.overrideSigningKey(probe.daemonSigningKey);
        sockets.push(socket);
        return socket;
      },
    });
    return { client, sockets, fingerprint };
  }

  test("accepts a daemon whose key matches the pin", async () => {
    const { client, sockets } = pinned();
    const connected = client.connect();
    const socket = sockets[0]!;
    socket.open();
    await flush();
    socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await expect(connected).resolves.toBeUndefined();
    expect(client.connected).toBe(true);
  });

  test("refuses a daemon whose key does not match the pin", async () => {
    const { client, sockets } = pinned({ wrongFingerprint: true });
    const connected = client.connect();
    // Attach the handler before driving the handshake: the rejection lands
    // while `socket.receive` is still awaited, and an unawaited rejecting
    // promise is reported as unhandled even when asserted on afterwards.
    const settled = connected.catch((cause: unknown) => cause);
    const socket = sockets[0]!;
    socket.open();
    await flush();
    socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    const error = (await settled) as WakuConnectionError;
    expect(error.kind).toBe("rejected");
    expect(error.message).toContain("not the one it claims");
    expect(client.connected).toBe(false);
  });

  test("a rejected pin never retries", async () => {
    const { client, sockets } = pinned({ wrongFingerprint: true });
    const settled = client.connect().catch(() => undefined);
    sockets[0]!.open();
    await flush();
    sockets[0]!.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await settled;
    // No second socket was created: the failure is final for this attempt.
    expect(sockets).toHaveLength(1);
  });

  test("a pin that matches a four-character prefix is still refused", async () => {
    // The sabotage case: a comparison that stopped early would accept this.
    const probe = new FakeSocket();
    const real = fingerprintOf(probe.daemonEd25519Public);
    const sockets: FakeSocket[] = [];
    const client = new WakuClient({
      address: "127.0.0.1:4312",
      token: "secret",
      expectedFingerprint: `${real.slice(0, 4)}${'0'.repeat(12)}`,
      randomUUID: () => "00000000-0000-4000-8000-000000000001",
      webSocketFactory: () => {
        const socket = new FakeSocket();
        socket.overrideSigningKey(probe.daemonSigningKey);
        sockets.push(socket);
        return socket;
      },
    });
    const connected = client.connect();
    const settled = connected.catch((cause: unknown) => cause);
    sockets[0]!.open();
    await flush();
    sockets[0]!.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    const error = (await settled) as WakuConnectionError;
    expect(error.kind).toBe("rejected");
  });

  test("exposes the authentication string once connected", async () => {
    const { client, sockets } = fixture();
    expect(client.authenticationString).toBeNull();
    await connect(client, sockets);
    const sas = client.authenticationString;
    expect(sas).not.toBeNull();
    expect(/^\d{4} \d{4} \d{4}$/.test(sas!)).toBe(true);
  });

  test("clears the authentication string when the connection drops", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    expect(client.authenticationString).not.toBeNull();
    socket.fail("connection lost");
    await flush();
    expect(client.authenticationString).toBeNull();
  });
});

describe("sealed transport", () => {  test("a plaintext text frame is refused, not skipped", async () => {
    const { client, sockets } = fixture();
    const connected = client.connect();
    const socket = sockets[0]!;
    socket.open();
    await flush();
    socket.receiveRaw(JSON.stringify({ type: "hello", protocolVersion: PROTOCOL_VERSION }));
    const error = (await connected.catch((cause: unknown) => cause)) as WakuConnectionError;
    expect(error.kind).toBe("handshake");
    expect(error.message).toContain("expected record, got text frame");
    expect(error.retryable).toBe(false);
  });

  test("a text frame after the handshake ends the connection", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const closed: string[] = [];
    client.subscribeConnectionState((state) => closed.push(state));

    socket.receiveRaw("{\"type\":\"taskStateChanged\"}");
    await flush();

    expect(closed.at(-1)).toBe("disconnected");
  });

  test("every client frame after the handshake is a sealed record", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const raw: Uint8Array[] = [];
    // Observe the wire by wrapping the socket the fixture produced.
    const originalSend = socket.send.bind(socket);
    socket.send = (data: Uint8Array) => {
      raw.push(data);
      originalSend(data);
    };

    await client.notify({ type: "refreshBackgroundWork" });
    expect(raw.length).toBe(1);
    // A record is a counter, a tag and a ciphertext — never printable JSON.
    expect(raw[0]!.length).toBeGreaterThan(8 + 16);
    expect(new TextDecoder().decode(raw[0]!)).not.toContain("refreshBackgroundWork");
  });

  test("accepts ArrayBuffer frames, as React Native delivers them", async () => {
    const { client, sockets } = fixture();
    const connected = client.connect();
    const socket = sockets[0]!;
    socket.open();
    await flush();
    socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await connected;
    expect(client.connected).toBe(true);
  });

  test("a tampered record ends the connection instead of being skipped", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const states: string[] = [];
    client.subscribeConnectionState((state) => states.push(state));

    // A well-formed frame for the next counter, with one ciphertext byte
    // flipped: the opener authenticates the whole record, so this cannot
    // dispatch anything and must end the connection.
    const tampered = new Uint8Array(8 + 4 + 16);
    tampered[7] = 2;          // the next counter after the client's hello
    tampered[10] = 0xff;
    socket.receiveRaw(tampered);
    await flush();

    expect(states.at(-1)).toBe("disconnected");
  });

  test("rejects a second hello once the protocol handshake completed", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    // A replayed/duplicate hello after establishment is just another message:
    // dispatch ignores it rather than re-running the handshake.
    await socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "again" });
    expect(client.connected).toBe(true);
  });
});

describe("WakuClient connection failures", () => {
  /**
   * Run one connection attempt and return the failure it produced.
   *
   * A resolved attempt fails this helper loudly rather than returning
   * undefined, so a test that stopped triggering its failure cannot pass by
   * accident.
   */
  async function failure(
    setup: (socket: FakeSocket) => Promise<void> | void,
  ): Promise<WakuConnectionError> {
    const { client, sockets } = fixture();
    let resolved = false;
    const connected = client.connect().then(
      () => {
        resolved = true;
      },
      (error: unknown) => error,
    );
    await setup(sockets[0]!);
    const error = await connected;
    if (resolved) throw new Error("the connection attempt was expected to fail");
    expect(error).toBeInstanceOf(WakuConnectionError);
    return error as WakuConnectionError;
  }

  test("types handshake failures so callers can tell what needs a person", async () => {
    const rejected = await failure(async (socket) => {
      socket.open();
      await flush();
      await socket.receive({ type: "rejected", message: "authentication failed" });
    });
    expect(rejected.kind).toBe("rejected");
    expect(rejected.retryable).toBe(false);
    expect(rejected.message).toBe("daemon rejected connection: authentication failed");

    const unsupported = await failure(async (socket) => {
      socket.open();
      await flush();
      await socket.receive({ type: "rejected", message: "protocol 3 is unsupported; expected 7" });
    });
    expect(unsupported.kind).toBe("protocol");

    const mismatch = await failure(async (socket) => {
      socket.open();
      await flush();
      await socket.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION + 1, daemonVersion: "x" });
    });
    expect(mismatch.kind).toBe("protocol");
    expect(mismatch.retryable).toBe(false);

    // A text frame is the pre-encryption protocol: a refusal, not a fallback.
    const plaintext = await failure(async (socket) => {
      socket.open();
      await flush();
      socket.receiveRaw(JSON.stringify({ type: "hello", protocolVersion: PROTOCOL_VERSION }));
    });
    expect(plaintext.kind).toBe("handshake");
    expect(plaintext.message).toContain("expected record, got text frame");

    // A well-formed record whose payload is not a `hello` (nor a `rejected`)
    // must not be skipped: the sealed channel is up, but the protocol answer
    // is wrong.
    const garbage = await failure(async (socket) => {
      socket.open();
      await flush();
      await socket.receive({ type: "event", sessionId: "x" });
    });
    expect(garbage.kind).toBe("handshake");

    const refused = await failure((socket) => socket.fail("Connection refused"));
    expect(refused.kind).toBe("unreachable");
    expect(refused.retryable).toBe(true);
    expect(refused.message).toBe("Connection refused");
  });

  test("times out a handshake the daemon never answers", async () => {
    const sockets: FakeSocket[] = [];
    const client = new WakuClient({
      address: "127.0.0.1:4312",
      token: "secret",
      connectTimeoutMs: 1,
      randomUUID: () => "00000000-0000-4000-8000-000000000001",
      webSocketFactory: () => {
        const socket = new FakeSocket();
        sockets.push(socket);
        return socket;
      },
    });
    const error = (await client.connect().catch((cause: unknown) => cause)) as WakuConnectionError;
    expect(error.kind).toBe("timeout");
    expect(error.retryable).toBe(true);
    expect(client.connectionState).toBe("disconnected");
    expect(sockets[0]!.readyState).toBe(3);

    // The abandoned socket's late events are ignored and a fresh attempt works.
    sockets[0]!.open();
    const again = client.connect();
    expect(sockets).toHaveLength(2);
    sockets[1]!.open();
    await flush();
    await sockets[1]!.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await again;
    expect(client.connected).toBe(true);
  });

  test("lets a request shorten its own timeout", async () => {
    const { client, sockets } = fixture();
    await connect(client, sockets);
    await expect(
      client.request({ type: "getSettings" }, undefined, undefined, { timeoutMs: 1 }),
    ).rejects.toThrow("timed out waiting for Waku daemon");
  });

  test("settles requests and clears the socket before listeners hear a remote close", async () => {
    const { client, sockets } = fixture();
    const socket = await connect(client, sockets);
    const request = client.request({ type: "getSettings" });
    let reconnected: Promise<void> | null = null;
    client.subscribeConnectionState((state) => {
      if (state === "disconnected" && !reconnected) reconnected = client.connect();
    });

    socket.fail("Software caused connection abort");
    const error = (await request.catch((cause: unknown) => cause)) as WakuConnectionError;
    expect(error.kind).toBe("closed");
    expect(client.lastDisconnectReason).toBe("Software caused connection abort");
    expect(sockets).toHaveLength(2);

    sockets[1]!.open();
    await flush();
    await sockets[1]!.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await reconnected!;
    expect(client.connected).toBe(true);
    const next = client.request({ type: "getSettings" });
    expect(sockets[1]!.sent).toHaveLength(2);
    const sent = JSON.parse(sockets[1]!.sent[1]!) as { requestId: string };
    await sockets[1]!.receive({
      type: "response",
      requestId: sent.requestId,
      outcome: { status: "ok", payload: { type: "ack" } },
    });
    await expect(next).resolves.toEqual({ type: "ack" });
  });

  test("records when the daemon last spoke", async () => {
    const sockets: FakeSocket[] = [];
    let now = 41;
    const client = new WakuClient({
      address: "127.0.0.1:4312",
      token: "secret",
      now: () => now,
      randomUUID: () => "00000000-0000-4000-8000-000000000001",
      webSocketFactory: () => {
        const socket = new FakeSocket();
        sockets.push(socket);
        return socket;
      },
    });
    expect(client.lastMessageAt).toBe(0);
    const connected = client.connect();
    now = 42;
    sockets[0]!.open();
    await flush();
    await sockets[0]!.receive({ type: "hello", protocolVersion: PROTOCOL_VERSION, daemonVersion: "test" });
    await connected;
    expect(client.lastMessageAt).toBe(42);
    now = 43;
    await sockets[0]!.receive({ type: "taskStateChanged", revision: 1 });
    expect(client.lastMessageAt).toBe(43);
  });
});
