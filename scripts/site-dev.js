/* Local preview of site/, the way Vercel serves it: static files, plus
   the likes function at /api/likes. Without real store credentials in
   the environment, the function talks to an in-memory stand-in for the
   Upstash REST API mounted at /__kv, so likes work offline and reset
   when this server stops.

     node scripts/site-dev.js          (PORT=1432 by default) */
import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import likes from '../site/api/likes.js';

const PORT = Number(process.env.PORT) || 1432;
const ROOT = path.join(import.meta.dirname, '..', 'site');
const TYPES = {
    '.html': 'text/html; charset=utf-8',
    '.css': 'text/css; charset=utf-8',
    '.js': 'text/javascript; charset=utf-8',
    '.json': 'application/json; charset=utf-8',
    '.ttf': 'font/ttf',
    '.png': 'image/png',
    '.svg': 'image/svg+xml',
    '.txt': 'text/plain; charset=utf-8',
};

if (!process.env.KV_REST_API_URL && !process.env.UPSTASH_REDIS_REST_URL) {
    process.env.KV_REST_API_URL = `http://127.0.0.1:${PORT}/__kv`;
    process.env.KV_REST_API_TOKEN = 'local-dev-token';
}

/* The four set commands the function uses and PUBLISH, nothing more.
   Subscribers hold an event stream open at /__kv/subscribe/<channel> and
   get each message as `data: message,<channel>,<payload>`, the way
   Upstash sends them. */
const sets = new Map();
const subscribers = new Map();
function redis([op, key, member]) {
    if (String(op).toUpperCase() === 'PUBLISH') {
        const listening = subscribers.get(key) || new Set();
        for (const res of listening) res.write(`data: message,${key},${member}\n\n`);
        return { result: listening.size };
    }
    if (!sets.has(key)) sets.set(key, new Set());
    const set = sets.get(key);
    switch (String(op).toUpperCase()) {
        case 'SADD': {
            const fresh = !set.has(member);
            set.add(member);
            return { result: fresh ? 1 : 0 };
        }
        case 'SREM':
            return { result: set.delete(member) ? 1 : 0 };
        case 'SCARD':
            return { result: set.size };
        case 'SISMEMBER':
            return { result: set.has(member) ? 1 : 0 };
        default:
            return { error: `ERR unknown command '${op}'` };
    }
}

function body(req) {
    return new Promise((resolve) => {
        let text = '';
        req.on('data', (chunk) => { text += chunk; });
        req.on('end', () => resolve(text));
    });
}

http.createServer(async (req, res) => {
    const url = decodeURIComponent(req.url.split('?')[0]);

    if (url === '/__kv/pipeline' && req.method === 'POST') {
        const authorised = req.headers.authorization === `Bearer ${process.env.KV_REST_API_TOKEN}`;
        res.writeHead(authorised ? 200 : 401, { 'Content-Type': 'application/json' });
        return res.end(authorised ? JSON.stringify(JSON.parse(await body(req)).map(redis)) : '{"error":"Unauthorized"}');
    }
    if (url.startsWith('/__kv/subscribe/')) {
        if (req.headers.authorization !== `Bearer ${process.env.KV_REST_API_TOKEN}`) {
            res.writeHead(401, { 'Content-Type': 'application/json' });
            return res.end('{"error":"Unauthorized"}');
        }
        const channel = url.slice('/__kv/subscribe/'.length);
        if (!subscribers.has(channel)) subscribers.set(channel, new Set());
        subscribers.get(channel).add(res);
        req.on('close', () => subscribers.get(channel).delete(res));
        res.writeHead(200, { 'Content-Type': 'text/event-stream', 'Cache-Control': 'no-store' });
        return res.write(`data: subscribe,${channel},1\n\n`);
    }
    if (url === '/api/likes') return likes(req, res);

    const file = path.join(ROOT, url.endsWith('/') ? `${url}index.html` : url);
    const hidden = file.startsWith(path.join(ROOT, 'api')) || path.basename(file) === 'vercel.json';
    if (!file.startsWith(ROOT) || hidden) {
        res.writeHead(404);
        return res.end('not found');
    }
    fs.readFile(file, (err, data) => {
        if (err) {
            res.writeHead(404);
            return res.end('not found');
        }
        res.writeHead(200, { 'Content-Type': TYPES[path.extname(file)] || 'application/octet-stream', 'Cache-Control': 'no-store' });
        res.end(data);
    });
}).listen(PORT, () => console.log(`Pravera site on http://localhost:${PORT}`));
