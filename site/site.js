/* Pravera homepage. No build step, no dependencies.

   Everything that moves here uses the app's own motion tokens
   (crates/pravera-ui/src/theme/motion.rs): 120 / 200 / 320 ms, ease-out,
   nothing bounces, and nothing moves unless something changed. */
(() => {
    'use strict';

    const REPO = 'n1ssyyy/Pravera';
    const MICRO = 120;
    const STANDARD = 200;
    const ENTRANCE = 320;
    const STAGGER = 22;

    const reduce = matchMedia('(prefers-reduced-motion: reduce)');
    const $ = (selector, root = document) => root.querySelector(selector);
    const $$ = (selector, root = document) => [...root.querySelectorAll(selector)];
    const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const easeOutCubic = (t) => 1 - Math.pow(1 - t, 3);

    /** Fades a line of text out, changes it, and fades it back. */
    function swap(node, text) {
        if (!node || node.textContent === text) return;
        if (reduce.matches) {
            node.textContent = text;
            return;
        }
        clearTimeout(node.swapping);
        node.classList.add('swap');
        node.swapping = setTimeout(() => {
            node.textContent = text;
            node.classList.remove('swap');
        }, MICRO);
    }

    const NS = 'http://www.w3.org/2000/svg';
    function icon(id) {
        const svg = document.createElementNS(NS, 'svg');
        svg.setAttribute('class', 'ico');
        svg.setAttribute('aria-hidden', 'true');
        const use = document.createElementNS(NS, 'use');
        use.setAttribute('href', `#${id}`);
        svg.append(use);
        return svg;
    }

    /* ---------------------------------------------------------------- */
    /* The route meter                                                   */
    /* ---------------------------------------------------------------- */

    /* The five shapes the app draws (components/route_meter.rs), as numbers
       so one can turn into another: how far the path is lifted into a
       detour, its weight, the gap either side of the midpoint, the ring and
       the dot that sit there, the dash, and how hollow the endpoints are. */
    const TONES = { direct: [0, 188, 125], relay: [255, 138, 31], off: [82, 82, 82] };
    const SHAPES = {
        cable: { lift: 0, weight: 3, gap: 0, ring: 0, dot: 0, dash: 0, hollow: 0, tone: 'direct' },
        lan: { lift: 0, weight: 2, gap: 0, ring: 0, dot: 0, dash: 0, hollow: 0, tone: 'direct' },
        tunnel: { lift: 0, weight: 1.5, gap: 3.5, ring: 1, dot: 0, dash: 0, hollow: 0, tone: 'direct' },
        relay: { lift: 1, weight: 1.25, gap: 0, ring: 0, dot: 1, dash: 0, hollow: 0, tone: 'relay' },
        broken: { lift: 0, weight: 1.25, gap: 0, ring: 0, dot: 0, dash: 3, hollow: 1, tone: 'off' },
    };
    const KEYS = ['lift', 'weight', 'gap', 'ring', 'dot', 'dash', 'hollow', 'r', 'g', 'b'];
    /* The app's meter, and the same drawing at the size of a diagram. */
    const SMALL = { w: 56, h: 16, x0: 3, x1: 53, base: 8, low: 12.75, apex: 2.5, k: 1 };
    const LARGE = { w: 560, h: 120, x0: 28, x1: 532, base: 60, low: 96, apex: 18, k: 3.5 };

    function numbers(shape) {
        const s = SHAPES[shape] || SHAPES.broken;
        const [r, g, b] = TONES[s.tone];
        return { ...s, r, g, b };
    }

    class Meter {
        constructor(svg, shape, geo = SMALL) {
            this.geo = geo;
            this.shape = shape;
            this.frame = 0;
            svg.setAttribute('viewBox', `0 0 ${geo.w} ${geo.h}`);
            svg.textContent = '';
            this.line = document.createElementNS(NS, 'path');
            this.ring = document.createElementNS(NS, 'circle');
            this.dot = document.createElementNS(NS, 'circle');
            this.ends = [document.createElementNS(NS, 'circle'), document.createElementNS(NS, 'circle')];
            svg.append(this.line, this.ring, this.dot, ...this.ends);
            this.now = numbers(shape);
            this.draw(this.now);
        }

        /** Turns the meter into another shape over `ms`. */
        set(shape, ms = STANDARD) {
            if (shape === this.shape) return;
            this.shape = shape;
            const from = this.now;
            const to = numbers(shape);
            cancelAnimationFrame(this.frame);
            if (reduce.matches || document.hidden || ms <= 0) {
                this.now = to;
                this.draw(to);
                return;
            }
            const start = performance.now();
            const step = (time) => {
                const t = Math.min(1, Math.max(0, (time - start) / ms));
                const e = easeOutCubic(t);
                const mix = {};
                for (const key of KEYS) mix[key] = from[key] + (to[key] - from[key]) * e;
                this.now = mix;
                this.draw(mix);
                if (t < 1) this.frame = requestAnimationFrame(step);
            };
            this.frame = requestAnimationFrame(step);
        }

        draw(p) {
            const { x0, x1, base, low, apex, k } = this.geo;
            const f = (n) => Number(n.toFixed(2));
            const mid = (x0 + x1) / 2;
            const yEnd = base + p.lift * (low - base);
            const yMid = base + p.lift * (apex - base);
            const color = `rgb(${Math.round(p.r)},${Math.round(p.g)},${Math.round(p.b)})`;
            /* A hollow endpoint is a ring, so the line stops at its edge. */
            const a = x0 + p.hollow * 2.25 * k;
            const b = x1 - p.hollow * 2.25 * k;
            const gap = p.gap * k;
            const yCut = yMid + (yEnd - yMid) * (gap / (mid - x0));
            const d = gap > 0.05
                ? `M${f(a)} ${f(yEnd)}L${f(mid - gap)} ${f(yCut)}M${f(mid + gap)} ${f(yCut)}L${f(b)} ${f(yEnd)}`
                : `M${f(a)} ${f(yEnd)}L${mid} ${f(yMid)}L${f(b)} ${f(yEnd)}`;

            const line = this.line;
            line.setAttribute('d', d);
            line.setAttribute('fill', 'none');
            line.setAttribute('stroke', color);
            line.setAttribute('stroke-width', f(p.weight * k));
            line.setAttribute('stroke-linejoin', 'round');
            if (p.dash > 0.05) line.setAttribute('stroke-dasharray', `${f(2.5 * k)} ${f(p.dash * k)}`);
            else line.removeAttribute('stroke-dasharray');

            const ring = this.ring;
            ring.setAttribute('cx', mid);
            ring.setAttribute('cy', f(yMid));
            ring.setAttribute('r', f(2 * k));
            ring.setAttribute('fill', 'none');
            ring.setAttribute('stroke', color);
            ring.setAttribute('stroke-width', f(1.25 * k));
            ring.setAttribute('opacity', f(p.ring));

            const dot = this.dot;
            dot.setAttribute('cx', mid);
            dot.setAttribute('cy', f(yMid));
            dot.setAttribute('r', f(1.9 * k));
            dot.setAttribute('fill', color);
            dot.setAttribute('stroke', 'none');
            dot.setAttribute('opacity', f(p.dot));

            this.ends.forEach((end, i) => {
                end.setAttribute('cx', i ? x1 : x0);
                end.setAttribute('cy', f(yEnd));
                end.setAttribute('r', f((2.25 - 0.4 * p.hollow) * k));
                end.setAttribute('fill', color);
                end.setAttribute('fill-opacity', f(1 - p.hollow));
                end.setAttribute('stroke', color);
                end.setAttribute('stroke-width', f(1.25 * k * p.hollow));
            });
        }
    }

    /* ---------------------------------------------------------------- */
    /* Hero: the headline, drawn once                                    */
    /* ---------------------------------------------------------------- */

    function initHero() {
        const fig = $('#hero-path');
        if (!fig) return;
        const svg = $('.path-draw', fig);
        const name = $('[data-route-name]', fig);
        const note = $('[data-route-note]', fig);
        if (reduce.matches) {
            new Meter(svg, 'cable', LARGE);
            return;
        }
        /* It opens on the detour everybody else takes, then goes straight. */
        const meter = new Meter(svg, 'relay', LARGE);
        fig.dataset.route = 'relay';
        name.textContent = 'Relayed';
        note.textContent = 'through a server you do not run';
        setTimeout(() => {
            fig.dataset.route = 'cable';
            meter.set('cable', ENTRANCE);
            swap(name, 'Direct');
            swap(note, 'the bits go straight there');
        }, 1900);
    }

    /* ---------------------------------------------------------------- */
    /* Devices: the app's home page, with a network that changes         */
    /* ---------------------------------------------------------------- */

    function initDevices() {
        const sheet = $('#devices');
        const list = $('#dev-list');
        if (!sheet || !list) return;
        const offlineLabel = $$('.group', list)[1];
        const event = $('#dev-event');
        const START = {
            studio: ['cable', 'USB4 on Ethernet 2'],
            homelab: ['lan', 'Local network'],
            thinkpad: ['tunnel', 'Tailscale, direct'],
            nas: ['broken', 'Offline'],
            attic: ['broken', 'Offline'],
        };
        const rows = {};
        for (const row of $$('.device', list)) {
            rows[row.dataset.device] = {
                row,
                label: $('[data-label]', row),
                meter: new Meter($('[data-meter]', row), row.dataset.route),
            };
        }

        const count = () => {
            const off = $$('.device[data-route="broken"]', list).length;
            $('#dev-online').textContent = $$('.device', list).length - off;
            $('#dev-offline').textContent = off;
        };
        const say = (text) => swap(event, text);
        const light = (row) => {
            row.classList.add('changed');
            clearTimeout(row.lit);
            row.lit = setTimeout(() => row.classList.remove('changed'), 1500);
        };
        /** A machine's route changes where it stands. */
        const route = (id, shape, text) => {
            const d = rows[id];
            d.row.dataset.route = shape;
            d.meter.set(shape);
            swap(d.label, text);
            light(d.row);
        };
        /** A machine comes up or goes down, so it changes group. */
        const move = async (id, shape, text) => {
            const d = rows[id];
            d.row.classList.add('gone');
            await wait(MICRO);
            if (shape === 'broken') list.append(d.row);
            else list.insertBefore(d.row, offlineLabel);
            d.row.dataset.route = shape;
            d.label.textContent = text;
            d.row.classList.remove('gone');
            d.row.style.setProperty('--i', 0);
            d.row.classList.remove('arriving');
            void d.row.offsetWidth;
            d.row.classList.add('arriving');
            d.meter.set(shape);
            count();
        };
        const source = (name, live, text) => {
            const li = $(`[data-source="${name}"]`);
            li.classList.toggle('live', live);
            swap($('em', li), text);
            $('#disc-live').textContent = $$('.sources .live').length;
        };

        const steps = [
            () => {
                say('Cable pulled from studio-pc. The session moved to the local network.');
                route('studio', 'lan', 'Local network');
                source('cable', false, 'nothing plugged in');
            },
            () => {
                say('thinkpad lost its direct path. Relayed until a new one is found.');
                route('thinkpad', 'relay', 'Tailscale, relayed');
            },
            () => {
                say('Hole punched to thinkpad. Direct again.');
                route('thinkpad', 'tunnel', 'Tailscale, direct');
            },
            () => {
                say('media-box woke up on the tailnet.');
                move('nas', 'tunnel', 'Tailscale, direct');
            },
            () => {
                say('Cable back in. studio-pc is on USB4 again.');
                route('studio', 'cable', 'USB4 on Ethernet 2');
                source('cable', true, 'USB4 on Ethernet 2');
            },
            () => {
                say('media-box went back to sleep.');
                move('nas', 'broken', 'Offline');
            },
        ];

        let at = 0;
        let timer = 0;
        let seen = false;
        const stop = () => {
            clearTimeout(timer);
            timer = 0;
        };
        const tick = () => {
            steps[at % steps.length]();
            at += 1;
            timer = setTimeout(tick, 3600);
        };
        const start = () => {
            if (timer || reduce.matches || !seen || document.hidden) return;
            timer = setTimeout(tick, 1800);
        };

        /** Scan: everything back to where it started, arriving in order. */
        const scan = () => {
            stop();
            at = 0;
            ['studio', 'homelab', 'thinkpad', 'nas', 'attic'].forEach((id, i) => {
                const d = rows[id];
                const [shape, text] = START[id];
                if (shape === 'broken') list.append(d.row);
                else list.insertBefore(d.row, offlineLabel);
                d.row.dataset.route = shape;
                d.label.textContent = text;
                d.meter.set(shape, 0);
                d.row.classList.remove('gone', 'changed', 'arriving');
                d.row.style.setProperty('--i', i);
                void d.row.offsetWidth;
                d.row.classList.add('arriving');
            });
            source('cable', true, 'USB4 on Ethernet 2');
            count();
            say('Scan finished. 3 online, 2 offline.');
            start();
        };
        $('#dev-scan').addEventListener('click', scan);

        if ('IntersectionObserver' in window) {
            new IntersectionObserver(([entry]) => {
                seen = entry.isIntersecting;
                if (seen) start();
                else stop();
            }, { threshold: 0.3 }).observe(sheet);
        }
        document.addEventListener('visibilitychange', () => (document.hidden ? stop() : start()));
    }

    /* ---------------------------------------------------------------- */
    /* Routes: take one away, the session falls to the next              */
    /* ---------------------------------------------------------------- */

    function initRoutes() {
        const ladder = $('#ladder');
        const fig = $('#route-path');
        if (!ladder || !fig) return;
        const big = new Meter($('.path-draw', fig), 'cable', LARGE);
        const name = $('[data-route-name]', fig);
        const note = $('[data-route-note]', fig);
        const reset = $('#route-reset');
        const rungs = [
            ['cable', 'cable', 'Cable'],
            ['lan', 'lan', 'Subnet'],
            ['tailnet', 'tunnel', 'Tailnet'],
            ['punch', 'tunnel', 'Hole punch'],
            ['relay', 'relay', 'Relay'],
        ].map(([id, shape, title]) => {
            const li = $(`[data-rung="${id}"]`, ladder);
            return {
                id,
                shape,
                title,
                li,
                toggle: $('.switch', li),
                state: $('[data-state]', li),
                meter: new Meter($('[data-meter]', li), shape),
                on: true,
            };
        });

        const paint = () => {
            const active = rungs.find((r) => r.on);
            for (const r of rungs) {
                r.toggle.setAttribute('aria-checked', String(r.on));
                r.li.classList.toggle('off', !r.on);
                r.li.classList.toggle('active', r === active);
                r.meter.set(r.on ? r.shape : 'broken');
                swap(r.state, !r.on ? 'unavailable' : r === active ? 'in use' : 'standing by');
            }
            const shape = active ? active.shape : 'broken';
            fig.dataset.route = shape;
            big.set(shape, ENTRANCE);
            swap(name, active ? active.title : 'No route');
            swap(note, !active
                ? 'the session waits for one to come back'
                : active.id === 'relay'
                    ? 'carries the session, still encrypted end to end'
                    : 'carries the session');
            reset.hidden = rungs.every((r) => r.on);
        };

        for (const r of rungs) {
            r.toggle.addEventListener('click', () => {
                r.on = !r.on;
                paint();
            });
        }
        reset.addEventListener('click', () => {
            rungs.forEach((r) => { r.on = true; });
            paint();
            rungs[0].toggle.focus();
        });
        paint();
    }

    /* ---------------------------------------------------------------- */
    /* Host: roles and grants, and a shell on the far machine            */
    /* ---------------------------------------------------------------- */

    function initHost() {
        const seg = $('#roles');
        const note = $('#grant-note');
        const grants = $$('#grants [data-grant]');
        if (!seg || !grants.length) return;
        const every = grants.map((s) => s.dataset.grant);
        /* The three built-in roles (crates/pravera-auth/src/permission.rs). */
        const ROLES = {
            viewer: ['view'],
            operator: ['view', 'control', 'clip-read', 'clip-write', 'audio', 'displays'],
            admin: every,
        };
        const NOTES = {
            viewer: 'The built-in viewer role: look, do not touch.',
            operator: 'The built-in operator role: everyday remote use, without file transfer, elevation or user management.',
            admin: 'The built-in admin role: everything, including who else gets in.',
            custom: 'A custom role. Every permission is its own grant: driving the desktop does not imply approving UAC, and the clipboard is granted one direction at a time.',
            blind: 'Without View screen this is not a usable session.',
        };
        const buttons = $$('[data-role]', seg);
        const held = () => grants.filter((s) => s.getAttribute('aria-checked') === 'true').map((s) => s.dataset.grant);
        const same = (a, b) => a.length === b.length && a.every((x) => b.includes(x));

        const show = (role, have) => {
            buttons.forEach((b, i) => {
                const on = b.dataset.role === role;
                b.setAttribute('aria-pressed', String(on));
                if (on) seg.style.setProperty('--at', i);
            });
            seg.classList.toggle('custom', !role);
            swap(note, !have.includes('view') ? NOTES.blind : NOTES[role || 'custom']);
        };

        seg.addEventListener('click', (e) => {
            const button = e.target.closest('[data-role]');
            if (!button) return;
            const want = ROLES[button.dataset.role];
            show(button.dataset.role, want);
            grants.forEach((s, i) => {
                setTimeout(() => s.setAttribute('aria-checked', String(want.includes(s.dataset.grant))),
                    reduce.matches ? 0 : i * STAGGER);
            });
        });
        for (const s of grants) {
            s.addEventListener('click', () => {
                s.setAttribute('aria-checked', String(s.getAttribute('aria-checked') !== 'true'));
                const have = held();
                show(Object.keys(ROLES).find((r) => same(ROLES[r], have)) || null, have);
            });
        }
    }

    function initTerm() {
        const term = $('#term');
        if (!term || reduce.matches || !('IntersectionObserver' in window)) return;
        const SCRIPT = [['hostname', 'homelab'], ['whoami', 'homelab\\you']];
        const prompt = () => {
            const p = document.createElement('p');
            const ps = document.createElement('span');
            ps.className = 'term-ps';
            ps.textContent = 'PS C:\\Users\\you>';
            const typed = document.createElement('span');
            const caret = document.createElement('span');
            caret.className = 'caret';
            p.append(ps, ' ', typed, caret);
            term.append(p);
            return { typed, caret };
        };
        const run = async () => {
            term.textContent = '';
            term.classList.add('live');
            for (const [command, answer] of SCRIPT) {
                const line = prompt();
                await wait(520);
                for (const ch of command) {
                    line.typed.textContent += ch;
                    await wait(58);
                }
                await wait(240);
                line.caret.remove();
                const out = document.createElement('p');
                out.className = 'term-out';
                out.textContent = answer;
                term.append(out);
            }
            prompt();
        };
        const io = new IntersectionObserver(([entry]) => {
            if (!entry.isIntersecting) return;
            io.disconnect();
            run();
        }, { threshold: 0.6 });
        io.observe(term);
    }

    /* ---------------------------------------------------------------- */
    /* Downloads and versions                                            */
    /* ---------------------------------------------------------------- */

    const PLATFORMS = {
        win: { name: 'Windows', file: 'Pravera-Setup-Windows-x64.exe', match: /windows/i },
        mac: { name: 'macOS', file: 'Pravera-Setup-macOS-arm64.zip', match: /macos/i },
        linux: { name: 'Linux', file: 'Pravera-Setup-Linux-x86_64.AppImage', match: /linux/i },
    };
    const megabytes = (bytes) => `${(bytes / 1048576).toFixed(1)} MB`;
    const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
    const day = (iso) => {
        const d = new Date(iso);
        return `${d.getDate()} ${MONTHS[d.getMonth()]} ${d.getFullYear()}`;
    };
    /** Newest version first. GitHub lists by creation time, which is not the same thing. */
    const byVersion = (a, b) => {
        const parts = (r) => r.tag.replace(/^v/, '').split('.').map((n) => parseInt(n, 10) || 0);
        const [x, y] = [parts(a), parts(b)];
        for (let i = 0; i < Math.max(x.length, y.length); i += 1) {
            if ((x[i] || 0) !== (y[i] || 0)) return (y[i] || 0) - (x[i] || 0);
        }
        return 0;
    };

    function system() {
        const platform = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || '';
        if (/android|iphone|ipad|ipod/i.test(navigator.userAgent || '')) return null;
        if (/win/i.test(platform)) return 'win';
        /* An iPad calls itself a Mac. */
        if (/mac/i.test(platform)) return navigator.maxTouchPoints > 1 ? null : 'mac';
        if (/linux|x11/i.test(platform)) return 'linux';
        return null;
    }

    /** Points the main buttons at the visitor's own system. */
    function paintDownloads() {
        const mine = system();
        if (!mine) return;
        const { name, file } = PLATFORMS[mine];
        for (const a of $$('[data-dl-main]')) {
            a.href = `https://github.com/${REPO}/releases/latest/download/${file}`;
            $('[data-dl-label]', a).textContent = `Download for ${name}`;
        }
        const plat = $(`.plat[data-plat="${mine}"]`);
        if (plat) {
            plat.classList.add('you');
            $('.plat-you', plat).hidden = false;
        }
        const meta = $('[data-dl-meta]');
        if (meta) {
            const version = $('[data-version]', meta);
            const size = document.createElement('span');
            size.dataset.size = mine;
            size.textContent = $(`.plat [data-size="${mine}"]`).textContent;
            const others = Object.keys(PLATFORMS).filter((k) => k !== mine).map((k) => PLATFORMS[k].name);
            meta.textContent = '';
            meta.append(version, ' · ', size, ` · also for ${others.join(' and ')}`);
        }
    }

    async function releases() {
        const KEY = 'pravera-releases';
        try {
            const hit = JSON.parse(sessionStorage.getItem(KEY));
            if (hit && Date.now() - hit.at < 10 * 60 * 1000) return hit.list;
        } catch { /* no storage, or nothing stored */ }
        const res = await fetch(`https://api.github.com/repos/${REPO}/releases?per_page=30`, {
            headers: { Accept: 'application/vnd.github+json' },
        });
        if (!res.ok) throw new Error(`GitHub answered ${res.status}`);
        const list = (await res.json())
            .filter((r) => !r.draft && !r.prerelease)
            .map((r) => ({
                tag: r.tag_name,
                date: r.published_at,
                url: r.html_url,
                assets: (r.assets || []).map((a) => ({ name: a.name, size: a.size })),
            }));
        try {
            sessionStorage.setItem(KEY, JSON.stringify({ at: Date.now(), list }));
        } catch { /* private window */ }
        return list;
    }

    function paintReleases(list) {
        const latest = list[0];
        $$('[data-version]').forEach((n) => { n.textContent = latest.tag; });
        $('#rel-tag').textContent = latest.tag;
        $('#rel-date').textContent = day(latest.date);
        $('#rel-notes-link').href = latest.url;
        for (const [key, { match }] of Object.entries(PLATFORMS)) {
            const asset = latest.assets.find((a) => match.test(a.name));
            if (asset) $$(`[data-size="${key}"]`).forEach((n) => { n.textContent = megabytes(asset.size); });
        }

        const SHOWN = 6;
        const ol = $('#rel-list');
        const more = $('#rel-more');
        ol.textContent = '';
        list.forEach((r, i) => {
            const li = document.createElement('li');
            li.className = 'rel arriving';
            li.style.setProperty('--i', Math.min(i, 10));
            if (i >= SHOWN) li.hidden = true;
            const a = document.createElement('a');
            a.href = r.url;
            const tag = document.createElement('b');
            tag.textContent = r.tag;
            const date = document.createElement('span');
            date.textContent = i === 0 ? `${day(r.date)}, latest` : day(r.date);
            const files = document.createElement('em');
            const total = r.assets.reduce((sum, x) => sum + x.size, 0);
            files.textContent = r.assets.length
                ? `${r.assets.length} ${r.assets.length === 1 ? 'file' : 'files'}, ${megabytes(total)}`
                : 'no files';
            a.append(tag, date, files, icon('i-out'));
            li.append(a);
            ol.append(li);
        });
        if (list.length > SHOWN) {
            const older = list.length - SHOWN;
            const label = $('#rel-more-label');
            more.hidden = false;
            more.setAttribute('aria-expanded', 'false');
            label.textContent = `Show ${older} older ${older === 1 ? 'version' : 'versions'}`;
            more.addEventListener('click', () => {
                const open = more.getAttribute('aria-expanded') !== 'true';
                more.setAttribute('aria-expanded', String(open));
                $$('.rel', ol).forEach((li, i) => {
                    if (i < SHOWN) return;
                    li.style.setProperty('--i', Math.min(i - SHOWN, 10));
                    li.hidden = !open;
                });
                label.textContent = open ? 'Show fewer' : `Show ${older} older ${older === 1 ? 'version' : 'versions'}`;
            });
        }
    }

    function initReleases() {
        if (!$('#rel-list')) return;
        releases().then((list) => {
            if (list.length) paintReleases([...list].sort(byVersion));
        }).catch(() => {
            const status = $('#rel-status');
            status.hidden = false;
            status.textContent = "Couldn't load the release list from GitHub. The download buttons still point at the latest release.";
        });
    }

    /* ---------------------------------------------------------------- */
    /* Likes                                                             */
    /* ---------------------------------------------------------------- */

    /** The like buttons stay hidden until /api/likes answers, so a host
     *  without the function just shows the page without them. A click
     *  paints at once and is put back if the server says no. After the
     *  first answer the count is live: the server pushes every change down
     *  one event stream, so a like from anyone shows here as it lands. */
    function initLikes() {
        const buttons = $$('[data-like]');
        const row = $('[data-like-row]');
        const msg = $('[data-like-msg]');
        if (!buttons.length) return;
        const compact = new Intl.NumberFormat('en', { notation: 'compact', maximumFractionDigits: 1 });
        let state = null;
        let busy = false;

        const call = async (method) => {
            const res = await fetch('api/likes', { method, headers: { Accept: 'application/json' } });
            const data = await res.json();
            if (!res.ok || typeof data.count !== 'number') throw new Error(data.error || 'likes unavailable');
            return { count: data.count, liked: Boolean(data.liked) };
        };
        const paint = (tick) => {
            for (const b of buttons) {
                b.hidden = false;
                b.setAttribute('aria-pressed', String(state.liked));
                const n = state.count === 1 ? '1 like' : `${state.count} likes`;
                b.setAttribute('aria-label', state.liked ? `Liked. Take your like back (${n})` : `Like Pravera (${n})`);
                const label = $('[data-like-label]', b);
                if (label) label.textContent = state.liked ? 'Liked' : 'Like Pravera';
                const count = $('[data-like-count]', b);
                count.textContent = compact.format(state.count);
                if (tick) {
                    count.classList.remove('tick');
                    void count.offsetWidth;
                    count.classList.add('tick');
                }
            }
            if (row) row.hidden = false;
        };
        const toggle = async () => {
            if (busy || !state) return;
            busy = true;
            const before = state;
            state = { count: Math.max(0, before.count + (before.liked ? -1 : 1)), liked: !before.liked };
            paint(true);
            if (msg) msg.textContent = '';
            try {
                state = await call(before.liked ? 'DELETE' : 'POST');
                if (msg) msg.textContent = state.liked ? 'Liked. Thanks for backing it.' : 'Like removed.';
            } catch {
                state = before;
                if (msg) msg.textContent = "That didn't save. Check your connection and try again.";
            }
            busy = false;
            paint(false);
        };

        /* The stream is only held open while somebody is here: it closes
           when the tab is hidden or has sat untouched for five minutes, and
           opens again on the next sign of life. Each (re)connection starts
           with the current count, so nothing is missed in between. */
        const live = () => {
            if (!('EventSource' in window)) return;
            const IDLE_MS = 5 * 60 * 1000;
            let source = null;
            let seen = Date.now();
            const close = () => {
                if (source) source.close();
                source = null;
            };
            const open = () => {
                if (source || document.hidden) return;
                source = new EventSource('api/likes');
                source.onmessage = (e) => {
                    let data;
                    try {
                        data = JSON.parse(e.data);
                    } catch {
                        return;
                    }
                    /* While this visitor's own click is in flight its answer decides. */
                    if (busy || typeof data.count !== 'number') return;
                    const next = { count: data.count, liked: 'liked' in data ? Boolean(data.liked) : state.liked };
                    if (next.count === state.count && next.liked === state.liked) return;
                    state = next;
                    paint(true);
                };
                /* The browser reconnects on its own after a dropped stream. If
                   the server refused outright, stop until the next sign of life. */
                source.onerror = () => {
                    if (source && source.readyState === EventSource.CLOSED) close();
                };
            };
            const awake = () => {
                seen = Date.now();
                open();
            };
            for (const type of ['pointerdown', 'pointermove', 'keydown', 'scroll']) {
                addEventListener(type, awake, { passive: true });
            }
            document.addEventListener('visibilitychange', () => (document.hidden ? close() : awake()));
            setInterval(() => {
                if (Date.now() - seen > IDLE_MS) close();
            }, 30000);
            open();
        };

        buttons.forEach((b) => b.addEventListener('click', toggle));
        call('GET').then((data) => {
            state = data;
            paint(false);
            live();
        }).catch(() => { /* no likes API on this host: keep the buttons hidden */ });
    }

    /* ---------------------------------------------------------------- */
    /* The window: rail and arriving sheets                              */
    /* ---------------------------------------------------------------- */

    /** One highlight in the rail, on the sheet that is on screen. */
    function initRail() {
        const rail = $('.rail');
        if (!rail) return;
        const items = $$('.rail-item', rail);
        const sheets = items.map((a) => $(a.getAttribute('href')));
        let queued = false;
        const paint = () => {
            queued = false;
            const line = innerHeight * 0.4;
            let at = -1;
            sheets.forEach((sheet, i) => {
                if (sheet && sheet.getBoundingClientRect().top <= line) at = i;
            });
            if (at >= 0 && innerHeight + scrollY >= document.documentElement.scrollHeight - 4) at = items.length - 1;
            items.forEach((a, i) => (i === at ? a.setAttribute('aria-current', 'true') : a.removeAttribute('aria-current')));
            rail.classList.toggle('on', at >= 0);
            if (at >= 0) rail.style.setProperty('--at', at);
        };
        const queue = () => {
            if (queued) return;
            queued = true;
            requestAnimationFrame(paint);
        };
        addEventListener('scroll', queue, { passive: true });
        addEventListener('resize', queue);
        paint();
    }

    function initReveal() {
        const nodes = $$('[data-reveal]');
        if (!('IntersectionObserver' in window) || reduce.matches) {
            nodes.forEach((n) => n.classList.add('in'));
            return;
        }
        const io = new IntersectionObserver((entries) => {
            for (const entry of entries) {
                if (!entry.isIntersecting) continue;
                entry.target.classList.add('in');
                io.unobserve(entry.target);
            }
        }, { rootMargin: '0px 0px -6% 0px', threshold: 0.03 });
        nodes.forEach((n) => io.observe(n));
    }

    /* ---------------------------------------------------------------- */
    /* Boot                                                              */
    /* ---------------------------------------------------------------- */

    /* Each part stands alone: one failing must not leave the page hidden. */
    for (const part of [initReveal, initRail, paintDownloads, initHero, initDevices, initRoutes, initHost, initTerm,
        initReleases, initLikes]) {
        try {
            part();
        } catch (error) {
            console.error(error);
            document.documentElement.classList.remove('js');
        }
    }
})();
