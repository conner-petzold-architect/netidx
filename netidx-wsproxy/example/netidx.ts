import WebSocket from "isomorphic-ws";
import { EventEmitter } from "events";

type NetidxValue =
  | { type: "U32"; value: number }
  | { type: "V32"; value: number }
  | { type: "I32"; value: number }
  | { type: "Z32"; value: number }
  | { type: "U64"; value: number }
  | { type: "V64"; value: number }
  | { type: "I64"; value: number }
  | { type: "Z64"; value: number }
  | { type: "F32"; value: number }
  | { type: "F64"; value: number }
  | { type: "DateTime"; value: number }
  | { type: "Duration"; value: string }
  | { type: "String"; value: string }
  | { type: "Boolean"; value: boolean }
  | { type: "Null" }
  | { type: "Ok" }
  | { type: "Error"; value: string }
  | { type: "Array"; value: NetidxValue[] }
  | { type: "Decimal"; value: number };

type NetidxEvent =
  | { type: "Unsubscribed" }
  | { type: "Update"; value: NetidxValue };

type Path = string;
type SubId = number;
type PubId = number;

type ToWs =
  | { type: "Subscribe"; path: Path }
  | { type: "Unsubscribe"; id: SubId }
  | { type: "Write"; id: SubId; val: NetidxValue }
  | { type: "Publish"; path: Path; init: NetidxValue }
  | { type: "Update"; id: PubId; val: NetidxValue }
  | { type: "Unpublish"; id: PubId }
  | { type: "Call"; path: Path; args: (string | NetidxValue)[][] };

type FromWs =
  | { type: "Subscribed"; id: SubId }
  | { type: "Update"; id: SubId; event: NetidxEvent }
  | { type: "Unsubscribed" }
  | { type: "Wrote" }
  | { type: "Published"; id: PubId }
  | { type: "Updated" }
  | { type: "Unpublished" }
  | { type: "Called"; result: NetidxValue }
  | { type: "Error"; error: String };

type UpdateListener = (value: NetidxEvent) => void;

const MESSAGE_PAIRS = {
  Subscribe: "Subscribed",
  Unsubscribe: "Unsubscribed",
  Write: "Wrote",
  Publish: "Published",
  Update: "Updated",
  Unpublish: "Unpublished",
  Call: "Called",
};

export class Netidx extends EventEmitter {
  private con: WebSocket;
  private openPromise: Promise<WebSocket.Event>;

  constructor(url: string | URL) {
    super();

    this.con = new WebSocket(url);

    this.openPromise = new Promise((resolve) => {
      this.con.addEventListener("open", resolve);
    });

    this.con.addEventListener("message", (event) => {
      const message: FromWs = JSON.parse(event.data as string);

      switch (message.type) {
        case "Update": {
          this.emit(message.id.toString(), message.event);
          break;
        }
      }
    });
  }

  private async send<To extends ToWs, From extends FromWs>(
    to: To
  ): Promise<From> {
    await this.openPromise;

    return new Promise((resolve) => {
      const listener = (event: WebSocket.MessageEvent) => {
        const from: From = JSON.parse(event.data as string);
        if (from.type === MESSAGE_PAIRS[to.type]) {
          resolve(from);
          this.con.removeEventListener("message", listener);
        }
      };

      this.con.addEventListener("message", listener);
      this.con.send(JSON.stringify(to));
    });
  }

  async subscribe(path: Path, listener: UpdateListener) {
    const message = await this.send<
      Extract<ToWs, { type: "Subscribe" }>,
      Extract<FromWs, { type: "Subscribed" }>
    >({
      type: "Subscribe",
      path,
    });

    if (message.type !== "Subscribed") {
      throw new Error("Unexpected message type");
    }

    this.on(message.id.toString(), listener);
    return () => {
      this.send<
        Extract<ToWs, { type: "Unsubscribe" }>,
        Extract<FromWs, { type: "Unsubscribed" }>
      >({ type: "Unsubscribe", id: message.id });
      this.off(message.id.toString(), listener);
    };
  }
}
