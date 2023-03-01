import { Netidx } from "./netidx";

async function main() {
  const nx = new Netidx("ws://127.0.0.1:4343/ws");
  const unsubA = await nx.subscribe("/local/bench/0/0", (v) => {
    console.log("a", v);
  });

  const unsubB = await nx.subscribe("/local/bench/0/0", (v) => {
    console.log("b", v);
  });

  unsubA();
  await new Promise((resolve) => setTimeout(resolve, 3000));
  unsubB();
}

main();
