/* Likes for the Pravera homepage. One Vercel function, one Redis set.

     GET    /api/likes  ->  { count, liked }
     POST   /api/likes  ->  like the project (counts once per visitor)
     DELETE /api/likes  ->  take the like back
     GET    /api/likes  with  Accept: text/event-stream
                        ->  the same answer as a live stream: one event
                            now, then one each time anybody's like lands

   The count is the size of a Redis set whose members are salted hashes
   of visitor addresses, so liking twice from the same place changes
   nothing and no address is ever stored. Storage is Upstash Redis over
   its REST API (the Vercel Marketplace integration sets the env vars),
   which keeps this file free of dependencies.

   Live updates are events, not polling: every change is published on a
   Redis channel, and each running copy of this function holds a single
   subscription to it that it shares between all the pages connected to
   that copy. */
import crypto from 'node:crypto';

const KEY = 'pravera:likes';
const CHANNEL = 'pravera-likes';
/* A stream ends itself after this long and the page reconnects, which
   keeps it inside the function's time limit (see vercel.json). */
const STREAM_MS = 50_000;
/* A comment line this often stops anything in between from deciding the
   connection is dead. */
const BEAT_MS = 15_000;

/** Where the store is. Vercel names the pair KV_REST_API_URL and
 *  KV_REST_API_TOKEN, Upstash names it UPSTASH_REDIS_REST_*, and a prefix
 *  typed when the store was connected lands in front of either
 *  (STORAGE_KV_REST_API_URL, ...). The token is always the URL's twin. */
function store() {
    const env = process.env;
    const urlName = ['KV_REST_API_URL', 'UPSTASH_REDIS_REST_URL'].find((k) => env[k])
        || Object.keys(env).sort().find((k) => /(_REST_API|_REDIS_REST)_URL$/.test(k) && env[k]);
    if (!urlName) return null;
    const token = env[urlName.replace(/_URL$/, '_TOKEN')];
    return token ? { url: env[urlName].replace(/\/$/, ''), token } : null;
}

/** Runs Redis commands in one round trip and returns their results. */
async function pipeline(kv, commands) {
    const res = await fetch(`${kv.url}/pipeline`, {
        method: 'POST',
        headers: { Authorization: `Bearer ${kv.token}`, 'Content-Type': 'application/json' },
        body: JSON.stringify(commands),
    });
    if (!res.ok) throw new Error(`store answered ${res.status}`);
    const rows = await res.json();
    const failed = rows.find((r) => r.error);
    if (failed) throw new Error(failed.error);
    return rows.map((r) => r.result);
}

/** A stable, anonymous id for whoever is asking. IPv6 is cut to its /64,
 *  since one device rotates through many addresses inside it. */
function visitor(req, salt) {
    const forwarded = String(req.headers['x-forwarded-for'] || '').split(',')[0].trim();
    let ip = String(req.headers['x-real-ip'] || forwarded || (req.socket && req.socket.remoteAddress) || '');
    if (ip.includes(':') && !ip.includes('.')) ip = ip.split(':').slice(0, 4).join(':');
    return crypto.createHash('sha256').update(`${salt}|${ip}`).digest('hex').slice(0, 32);
}

/** Likes only count when they come from the page itself. */
function fromThisSite(req) {
    const site = req.headers['sec-fetch-site'];
    if (site && site !== 'same-origin' && site !== 'none') return false;
    const origin = req.headers.origin;
    if (!origin) return true;
    try {
        return new URL(origin).host === req.headers.host;
    } catch {
        return false;
    }
}

/* ------------------------------ live ------------------------------ */

/* The pages connected to this copy of the function, and its one
   subscription to the store. */
const hub = { clients: new Set(), upstream: null, ready: null };

/** Opens the subscription if there is none. Resolves true once the store
 *  has accepted it, false if it could not be opened. */
function listen(kv) {
    if (hub.ready) return hub.ready;
    const upstream = new AbortController();
    const mark = `message,${CHANNEL},`;
    hub.upstream = upstream;
    hub.ready = new Promise((resolve) => {
        (async () => {
            try {
                const res = await fetch(`${kv.url}/subscribe/${CHANNEL}`, {
                    method: 'POST',
                    headers: { Authorization: `Bearer ${kv.token}`, Accept: 'text/event-stream' },
                    signal: upstream.signal,
                });
                if (!res.ok || !res.body) throw new Error(`store answered ${res.status}`);
                resolve(true);
                const text = new TextDecoder();
                let buffer = '';
                for await (const chunk of res.body) {
                    buffer += text.decode(chunk, { stream: true });
                    let cut;
                    while ((cut = buffer.indexOf('\n')) >= 0) {
                        const line = buffer.slice(0, cut).trim();
                        buffer = buffer.slice(cut + 1);
                        const at = line.indexOf(mark);
                        if (!line.startsWith('data:') || at < 0) continue;
                        let change;
                        try {
                            change = JSON.parse(line.slice(at + mark.length));
                        } catch {
                            continue;
                        }
                        for (const client of hub.clients) client.push(change);
                    }
                }
            } catch {
                /* refused, dropped, or closed by hang() below */
            }
            resolve(false);
            /* The subscription is gone. Let the pages reconnect and start
               a fresh one, unless a newer one has already replaced it. */
            if (hub.upstream !== upstream) return;
            hub.upstream = null;
            hub.ready = null;
            for (const client of [...hub.clients]) client.end();
        })();
    });
    return hub.ready;
}

/** Drops the subscription once nobody is listening. */
function hang() {
    if (hub.clients.size || !hub.upstream) return;
    const upstream = hub.upstream;
    hub.upstream = null;
    hub.ready = null;
    upstream.abort();
}

/** Holds the response open and writes one event per change. A visitor is
 *  told `liked` only for their own changes, so a second tab of theirs
 *  follows along; everybody else just gets the new count. Resolves false
 *  if the stream could not be started, otherwise when it has closed. */
async function stream(req, res, kv, id) {
    const client = { push() {}, end() {} };
    hub.clients.add(client);
    let done;
    const closed = new Promise((resolve) => { done = resolve; });
    let timers = [];
    const leave = () => {
        timers.forEach(clearTimeout);
        timers = [];
        hub.clients.delete(client);
        hang();
        done(true);
    };
    req.on('close', leave);

    if (!(await listen(kv))) {
        leave();
        return false;
    }

    res.writeHead(200, {
        'Content-Type': 'text/event-stream; charset=utf-8',
        'Cache-Control': 'no-store, no-transform',
        'X-Accel-Buffering': 'no',
    });
    const event = (data) => res.write(`data: ${JSON.stringify(data)}\n\n`);
    client.push = (change) => {
        if (typeof change.count !== 'number') return;
        event(change.id === id ? { count: change.count, liked: Boolean(change.liked) } : { count: change.count });
    };
    client.end = () => res.end();
    res.write('retry: 1000\n\n');

    try {
        const [count, liked] = await pipeline(kv, [['SCARD', KEY], ['SISMEMBER', KEY, id]]);
        event({ count, liked: liked === 1 });
    } catch {
        res.end();
        return closed;
    }
    const beat = () => {
        res.write(': beat\n\n');
        timers.push(setTimeout(beat, BEAT_MS));
    };
    timers.push(setTimeout(beat, BEAT_MS), setTimeout(() => res.end(), STREAM_MS));
    return closed;
}

/* ------------------------------ handler ------------------------------ */

export default async function handler(req, res) {
    const send = (status, body) => {
        res.statusCode = status;
        res.setHeader('Content-Type', 'application/json; charset=utf-8');
        res.setHeader('Cache-Control', 'no-store');
        res.end(JSON.stringify(body));
    };

    if (!['GET', 'POST', 'DELETE'].includes(req.method)) {
        res.setHeader('Allow', 'GET, POST, DELETE');
        return send(405, { error: 'Use GET, POST or DELETE.' });
    }
    const kv = store();
    if (!kv) return send(503, { error: 'Likes storage is not connected yet.' });
    if (req.method !== 'GET' && !fromThisSite(req)) {
        return send(403, { error: 'Likes can only be left from the Pravera site.' });
    }

    const id = visitor(req, process.env.LIKES_SALT || kv.token);
    if (req.method === 'GET' && String(req.headers.accept || '').includes('text/event-stream')) {
        const opened = await stream(req, res, kv, id);
        if (!opened) send(502, { error: "Couldn't reach the likes store. Try again in a moment." });
        return undefined;
    }
    try {
        if (req.method === 'GET') {
            const [count, liked] = await pipeline(kv, [['SCARD', KEY], ['SISMEMBER', KEY, id]]);
            return send(200, { count, liked: liked === 1 });
        }
        const liking = req.method === 'POST';
        const [changed, count] = await pipeline(kv, [[liking ? 'SADD' : 'SREM', KEY, id], ['SCARD', KEY]]);
        if (changed) {
            /* Tell every open page. The like itself is already saved, so a
               failed announcement must not fail the request. */
            await pipeline(kv, [['PUBLISH', CHANNEL, JSON.stringify({ count, id, liked: liking })]]).catch(() => {});
        }
        return send(200, { count, liked: liking });
    } catch {
        return send(502, { error: "Couldn't reach the likes store. Try again in a moment." });
    }
}
