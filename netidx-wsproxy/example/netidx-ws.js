class Netidx {
    con = null;
    subs = new Map();
    pending_subs = [];

    constructor(url) {
        this.con = new WebSocket(url);
        this.con.addEventListener('open', (event) => {
            for (sub of pending_subs) {
                this.con.send(JSON.stringify({'type': 'Subscribe', 'path': sub[0]}));
            }
        });
        this.con.addEventListener('message', (event) => {
            console.log("message from netidx", event.data);
            const ev = JSON.parse(event.data);
            if (ev.type == "Subscribed") {
                let handler = this.pending_subs.pop()[1];
                let w = this.subs.get(ev.id);
                if (w == undefined) {
                    this.subs.set(ev.id, [handler]);
                } else {
                    w.push(handler);
                }
            } else if (ev.type == "Update") {
                let w = this.subs.get(ev.id);
                if (w != undefined) {
                    let i = 0;
                    while (i < w.length) {
                        if (!w[i](ev.event)) {
                            w.splice(i, 1);
                        } else {
                            i = i + 1;
                        }
                    }
                    if (w.length == 0) {
                        this.con.send(JSON.stringify({'type': 'Unsubscribe', 'id': ev.id }))
                    }
                }
            }
        });
    }

    subscribe(path, handler) {
        this.pending_subs.unshift([path, handler]);
        if(this.con.readyState == 1) {
            this.con.send(JSON.stringify({ 'type': 'Subscribe', 'path': path }));
        }
    }
}

nx = new Netidx("ws://127.0.0.1:4343/ws");
nx.subscribe('/local/bench/0/0', (v) => {
    console.log(v);
    true
});
