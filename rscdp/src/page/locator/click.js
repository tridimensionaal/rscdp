function readiness(selector, budgetMs, desiredChecked) {
    const target = this;
    const wait = reason => ({ status: 'wait', reason });
    const unsupported = reason => ({ status: 'unsupported', reason });
    const rect = r => ({ left: r.left, top: r.top, right: r.right, bottom: r.bottom });
    const transformed = style => style.transform !== 'none'
        || style.perspective !== 'none'
        || (style.translate && style.translate !== 'none')
        || (style.rotate && style.rotate !== 'none')
        || (style.scale && style.scale !== 'none')
        || (style.offsetPath && style.offsetPath !== 'none')
        || (style.zoom && Number(style.zoom) !== 1 && style.zoom !== 'normal');
    const visible = node => {
        const style = node.ownerDocument.defaultView.getComputedStyle(node);
        return node.isConnected && style.visibility !== 'hidden' && style.visibility !== 'collapse'
            && Array.from(node.getClientRects()).some(r => r.width > 0 && r.height > 0);
    };
    function preflight() {
        if (!target.isConnected) return { status: 'detached' };
        if (selector !== null) {
            const matches = target.ownerDocument.querySelectorAll(selector);
            if (matches.length > 1) return { status: 'ambiguous', count: matches.length };
            if (matches.length !== 1 || matches[0] !== target) return { status: 'detached' };
        }
        if (typeof desiredChecked === 'boolean') {
            const view = target.ownerDocument.defaultView;
            if (!view || !(target instanceof view.HTMLInputElement) || target.type !== 'checkbox')
                return unsupported('set_checked requires a native checkbox input');
            if (target.checked === desiredChecked) return { status: 'checked' };
        }
        if (!visible(target)) return wait('element is not visible');
        if (target.matches(':disabled')) return wait('native control is disabled');
        return null;
    }
    function measure(hitTest) {
        const invalid = preflight();
        if (invalid) return invalid;
        const levels = [];
        const clips = [];
        const overflowClips = [];
        const signature = [];
        let node = target;
        let view = target.ownerDocument.defaultView;
        let dx = 0, dy = 0;
        // Each offset maps a point in the target's document into this ancestor
        // viewport. Only untranslated, unscaled iframe content axes are supported.
        for (;;) {
            const viewport = view.visualViewport;
            if (viewport && (viewport.scale !== 1 || viewport.offsetLeft !== 0 || viewport.offsetTop !== 0))
                return unsupported('pinch-zoomed or offset visual viewports are unsupported');
            if (!visible(node)) return wait('ancestor frame is not visible');
            levels.push({ node, view, dx, dy });
            clips.push({ left: -dx, top: -dy, right: view.innerWidth - dx, bottom: view.innerHeight - dy });
            signature.push(dx, dy, view.innerWidth, view.innerHeight, view.scrollX, view.scrollY);
            for (let ancestor = node.parentElement; ancestor; ancestor = ancestor.parentElement) {
                const style = view.getComputedStyle(ancestor);
                const clipX = style.overflowX !== 'visible';
                const clipY = style.overflowY !== 'visible';
                if (!clipX && !clipY) continue;
                for (let parent = ancestor; parent; parent = parent.parentElement) {
                    if (transformed(view.getComputedStyle(parent)))
                        return unsupported('transformed clipping ancestors are unsupported');
                }
                const box = ancestor.getBoundingClientRect();
                overflowClips.push({
                    left: clipX ? box.left + ancestor.clientLeft - dx : -Infinity,
                    right: clipX ? box.left + ancestor.clientLeft + ancestor.clientWidth - dx : Infinity,
                    top: clipY ? box.top + ancestor.clientTop - dy : -Infinity,
                    bottom: clipY ? box.top + ancestor.clientTop + ancestor.clientHeight - dy : Infinity,
                });
            }
            if (view === view.top) break;
            let frame, parent;
            try {
                frame = view.frameElement;
                parent = view.parent;
                // frameElement is null for inaccessible ancestry, even if parent
                // itself can be named. Reading document also checks same origin.
                if (!frame || !parent.document)
                    return unsupported('cross-origin or remote iframe ancestry is unsupported');
            } catch (_) {
                return unsupported('cross-origin iframe ancestry is unsupported');
            }
            if (frame.tagName !== 'IFRAME') return unsupported('only iframe embedding is supported');
            for (let ancestor = frame; ancestor; ancestor = ancestor.parentElement) {
                if (transformed(parent.getComputedStyle(ancestor)))
                    return unsupported('iframe transforms, perspective, and CSS zoom are unsupported');
            }
            const style = parent.getComputedStyle(frame);
            if (['paddingLeft', 'paddingRight', 'paddingTop', 'paddingBottom'].some(p => parseFloat(style[p]) !== 0))
                return unsupported('iframe padding is unsupported');
            const box = frame.getBoundingClientRect();
            const left = box.left + parseFloat(style.borderLeftWidth);
            const top = box.top + parseFloat(style.borderTopWidth);
            dx += left;
            dy += top;
            clips.push({ left: left - dx, top: top - dy,
                right: box.right - parseFloat(style.borderRightWidth) - dx,
                bottom: box.bottom - parseFloat(style.borderBottomWidth) - dy });
            signature.push(box.left, box.top, box.right, box.bottom);
            node = frame;
            view = parent;
        }
        const bounds = { left: -Infinity, top: -Infinity, right: Infinity, bottom: Infinity };
        for (const clip of clips) {
            bounds.left = Math.max(bounds.left, clip.left);
            bounds.top = Math.max(bounds.top, clip.top);
            bounds.right = Math.min(bounds.right, clip.right);
            bounds.bottom = Math.min(bounds.bottom, clip.bottom);
        }
        signature.push(bounds.left, bounds.top, bounds.right, bounds.bottom);
        // Overflow does not clip every descendant (fixed/absolute containing
        // blocks and root propagation matter). Probe these regions as additional
        // candidates, never exclude viewport candidates based on overflow alone.
        // The browser's hit tests below remain the authority on actual clipping.
        const intersect = (a, b) => ({ left: Math.max(a.left, b.left), top: Math.max(a.top, b.top),
            right: Math.min(a.right, b.right), bottom: Math.min(a.bottom, b.bottom) });
        const regions = [bounds];
        let accumulated = bounds;
        for (const clip of overflowClips) {
            signature.push(clip.left, clip.top, clip.right, clip.bottom);
            regions.push(intersect(bounds, clip));
            accumulated = intersect(accumulated, clip);
            regions.push(accumulated);
        }
        const boxes = Array.from(target.getClientRects(), rect);
        for (const box of boxes) signature.push(box.left, box.top, box.right, box.bottom);
        if (!hitTest) return { status: 'ready' };
        for (const box of boxes) for (const region of regions) {
            const left = Math.max(box.left, region.left), right = Math.min(box.right, region.right);
            const top = Math.max(box.top, region.top), bottom = Math.min(box.bottom, region.bottom);
            if (right <= left || bottom <= top) continue;
            // A bounded candidate set is conservative around nonrectangular clips
            // and overlays: never dispatch unless every document confirms a hit.
            for (const [fx, fy] of [[.5, .5], [.2, .2], [.8, .2], [.2, .8], [.8, .8]]) {
                const x = left + (right - left) * fx, y = top + (bottom - top) * fy;
                const clear = levels.every(level => {
                    const hit = level.view.document.elementFromPoint(x + level.dx, y + level.dy);
                    return hit && (hit === level.node || level.node.contains(hit));
                });
                if (clear) return { status: 'ready', x: x + dx, y: y + dy, signature };
            }
        }
        return wait('element is clipped or covered at every candidate point');
    }
    const initial = measure(false);
    if (initial.status !== 'ready') return initial;
    target.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
    return new Promise((resolve, reject) => {
        const view = target.ownerDocument.defaultView;
        let frame = 0;
        const finish = (result, error) => {
            view.clearTimeout(timer);
            view.cancelAnimationFrame(frame);
            if (error) reject(error); else resolve(result);
        };
        const timer = view.setTimeout(() => finish(wait('animation frame sampling timed out')),
            Math.max(1, Math.min(250, budgetMs)));
        frame = view.requestAnimationFrame(() => {
            try {
                const first = measure(true);
                if (first.status !== 'ready') return finish(first);
                frame = view.requestAnimationFrame(() => {
                    try {
                        const second = measure(true);
                        if (second.status !== 'ready') return finish(second);
                        const stable = first.signature.length === second.signature.length
                            && first.signature.every((value, index) => value === second.signature[index])
                            && first.x === second.x && first.y === second.y;
                        if (!stable) return finish(wait('element or ancestor geometry is moving'));
                        finish({ status: 'ready', x: second.x, y: second.y });
                    } catch (error) { finish(null, error); }
                });
            } catch (error) { finish(null, error); }
        });
    });
}
