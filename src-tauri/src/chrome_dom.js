(() => {
  const all = (selector, root = document) => [...root.querySelectorAll(selector)];
  const text = element => element?.innerText?.trim() ?? "";
  const visible = element => !!element?.getClientRects().length &&
    getComputedStyle(element).visibility !== "hidden";
  const first = (selector, root) => all(selector, root).find(visible);
  const button = (label, root) => all("button, [role=button]", root).find(element =>
    visible(element) && [element.getAttribute("aria-label"), element.title, text(element)].some(value =>
      value?.trim().toLowerCase() === label.toLowerCase()));
  const enabled = element => !!element && !element.disabled && element.getAttribute("aria-disabled") !== "true";
  const click = element => {
    if (!enabled(element)) return false;
    element.scrollIntoView({block: "center"});
    element.click();
    return true;
  };
  const fill = (input, value) => {
    if (!input) return false;
    input.focus();
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set.call(input, value);
    input.dispatchEvent(new Event("input", {bubbles: true}));
    input.dispatchEvent(new Event("change", {bubbles: true}));
    return true;
  };
  const modal = () => first(".modal.showBackground");
  const cards = () => all(".searchList__item").filter(row => row.querySelector(".searchItem__trackItem"));
  const playlistKey = href => {
    if (!href) return null;
    let url;
    try { url = new URL(href, location.href); } catch { return null; }
    const path = url.pathname.match(/^\/([\w-]+)\/sets\/([\w-]+)(?:\/s-[\w-]+)?\/?$/);
    return url.origin === "https://soundcloud.com" && path ? url.origin + "/" + path[1] + "/sets/" + path[2] : null;
  };
  const playlistRow = (name, url) => {
    const rows = all(".addToPlaylistList__item", modal()).filter(row => {
      const link = row.querySelector(".addToPlaylistItem__titleLink");
      return visible(row) && (url ? playlistKey(link?.href) === url : text(link) === name);
    });
    return rows.length === 1 ? rows[0] : null;
  };
  const duration = root => {
    const element = root?.querySelector('time, [data-duration-ms], [class*="Duration"], [class*="duration"]');
    const millis = Number(element?.getAttribute("data-duration-ms") ?? 0);
    if (millis > 0) return Math.round(millis / 1000);
    const clock = [element?.getAttribute("datetime"), element?.title, text(element)]
      .map(value => value?.match(/(?:^|\b)(\d{1,3}:\d{2}(?::\d{2})?)\b/)?.[1]).find(Boolean);
    if (clock) return clock.split(":").reduce((seconds, part) => seconds * 60 + Number(part), 0);
    const iso = element?.getAttribute("datetime")?.match(/^PT(?:(\d+)H)?(?:(\d+)M)?(?:(\d+)S)?$/i);
    return iso ? Number(iso[1] ?? 0) * 3600 + Number(iso[2] ?? 0) * 60 + Number(iso[3] ?? 0) : null;
  };
  const added = row => row && (button("Added", row) || button("Remove from Playlist", row));
  const privateRadio = () => modal()?.querySelector('input[type="radio"][value="private"]');
  return {
    captcha() {
      const host = /(^|\.)(captcha-delivery\.com|smartcaptcha\.yandexcloud\.net)$|^captcha-api\.yandex\./i;
      if (host.test(location.hostname) || /\/showcaptcha(?:\/|$)/i.test(location.pathname)) return true;
      if (all("iframe[src]").some(frame => visible(frame) &&
        (host.test(new URL(frame.src, location.href).hostname) ||
         /captcha challenge|проверка безопасности/i.test(frame.title)))) return true;
      const hint = /verify (that )?you are (a )?human|slide.{0,40}(right|puzzle)|подтвердите.{0,30}вы не робот|передвиньте.{0,25}ползунок|перетащите.{0,25}ползунок/i;
      return all('h1,h2,[role="heading"]').some(heading => visible(heading) && hint.test(text(heading)));
    },
    source() {
      const tracks = all('a[href*="/track/"]').map(anchor => {
        let row = anchor.parentElement;
        while (row && !row.querySelector('a[href^="/artist/"]')) row = row.parentElement;
        return {url: anchor.href, title: text(anchor), duration: duration(row),
          artists: row ? all('a[href^="/artist/"]', row).map(text).filter(Boolean) : []};
      }).filter(track => track.title && track.artists.length);
      return {title: text(first("h1")), tracks};
    },
    scroll() {
      for (const element of all("*")) {
        if (element.clientHeight > 100 && element.scrollHeight > element.clientHeight + 100) {
          element.scrollTop += Math.max(400, element.clientHeight * 0.8);
        }
      }
      window.scrollBy(0, innerHeight * 0.8);
      return true;
    },
    search(query) {
      if (location.pathname !== "/search/sounds" || new URLSearchParams(location.search).get("q") !== query) return null;
      if (/Sorry we didn['’]t find any results for/i.test(document.body?.innerText ?? "")) return {missing: true, rows: []};
      const durations = new Map();
      if (Array.isArray(window.__sc_hydration)) {
        for (const entry of window.__sc_hydration) {
          const data = entry?.data;
          const items = Array.isArray(data?.collection) ? data.collection : [data];
          for (const item of items) {
            if (item?.kind === "track" && typeof item.permalink_url === "string" && Number.isFinite(item.duration)) {
              durations.set(item.permalink_url, Math.round(item.duration / 1000));
            }
          }
        }
      }
      const rows = cards().map((row, index) => {
        const url = row.querySelector("a.soundTitle__title")?.href ?? null;
        return {
          index, title: text(row.querySelector(".soundTitle__title")),
          artist: text(row.querySelector(".soundTitle__username")),
          url, duration: duration(row) ?? durations.get(url?.split(/[?#]/)[0]) ?? null,
          unavailable: !!row.querySelector('[title="Unavailable"]') || text(row).includes("Not available"),
        };
      });
      if (!rows.length || rows.some(row => !row.unavailable && (!row.title || !row.artist))) return null;
      return {missing: false, rows: rows.filter(row => !row.unavailable)};
    },
    more(index, artist, title, url) {
      const row = cards()[index];
      return !!row && text(row.querySelector(".soundTitle__title")) === title &&
        text(row.querySelector(".soundTitle__username")) === artist &&
        (!url || row.querySelector("a.soundTitle__title")?.href?.split(/[?#]/)[0].replace(/\/$/, "") === url) &&
        click(button("More", row));
    },
    addMenu() { return click(button("Add to Playlist")); },
    trackMore(url) {
      if (location.href.split(/[?#]/)[0].replace(/\/$/, "") !== url) return false;
      const actions = first(".listenEngagement, .soundActions, .listenEngagement__actions");
      return !!actions && click(button("More", actions) || first("button.sc-button-more", actions));
    },
    dialog() { return !!modal()?.querySelector(".tabs__tab"); },
    createTab() {
      const root = modal();
      return !!root && click(all(".tabs__tab, button, [role=tab], a", root).find(element =>
        visible(element) && text(element) === "Create a playlist"));
    },
    title(value) {
      const root = modal();
      if (!root) return false;
      const label = all("label", root).find(element => text(element) === "Playlist title");
      return fill(label?.control || first('input[placeholder="Playlist title"],input[aria-label="Playlist title"],input[type="text"]', root), value);
    },
    private() {
      const input = privateRadio();
      return !!input && (input.checked || click(input.labels?.[0] || input.closest("label") ||
        all("label", modal()).find(label => text(label) === "Private")));
    },
    filter(value) {
      const root = modal();
      return !!root && fill(first('input[placeholder="Filter playlists"]', root), value);
    },
    saveState(name, created, url, full) {
      const root = modal();
      if (!root) return {state: "hidden"};
      if (created) {
        const row = playlistRow(name, url);
        if (added(row)) return {state: "added"};
        if (row && (button("Add to Playlist", row) || full)) return {state: "ready"};
      } else if (privateRadio()?.checked && button("Save", root) &&
        all("input", root).some(input => input.value === name)) {
        return {state: "ready"};
      }
      return {state: "waiting"};
    },
    save(name, created, url) {
      const root = modal();
      if (!root) return false;
      if (created) {
        const row = playlistRow(name, url);
        return !!row && click(button("Add to Playlist", row));
      }
      return !!privateRadio()?.checked && all("input", root).some(input => input.value === name) && click(button("Save", root));
    },
    membership(name, url, allowFull) {
      const row = modal() && playlistRow(name, url);
      if (!row) return null;
      if (button("Remove from Playlist", row)) return "removable";
      if (added(row)) return "present";
      const add = button("Add to Playlist", row);
      if (enabled(add) || (allowFull && add)) return "absent";
      return null;
    },
    remove(name, url) {
      const row = modal() && playlistRow(name, url);
      return !!row && click(button("Remove from Playlist", row));
    },
    validation() {
      const root = modal();
      const message = root && all('[role="alert"], .textfield__validation, .g-input-validation', root)
        .filter(visible).map(text).find(Boolean);
      return message ? {message: message.slice(0, 300), invalid: !!root.querySelector('[aria-invalid="true"]')} : null;
    },
    links(name) {
      return all('a[href*="/sets/"]').filter(anchor => text(anchor) === name).map(anchor => anchor.href);
    },
    library() {
      if (location.pathname.replace(/\/$/, "") !== "/you/sets") return [];
      return all('a[href*="/sets/"]').map(anchor => ({name: text(anchor), url: playlistKey(anchor.href)}))
        .filter(row => row.name && row.url);
    },
    playlistDetails(expected) {
      if (playlistKey(location.href) !== expected) return null;
      const edit = first(".sc-button-edit, .systemPlaylistDetails__edit") || button("Edit playlist") || button("Edit");
      if (!edit) return null;
      const data = Array.isArray(window.__sc_hydration) && window.__sc_hydration
        .map(item => item?.data)
        .find(item => (item?.kind === "playlist" || Number.isInteger(item?.track_count)) &&
          playlistKey(item.permalink_url) === expected);
      const heading = first(".soundTitle__title, h1");
      const name = typeof data?.title === "string" ? data.title.trim() : text(heading);
      const countText = all(".genericTrackCount, .systemPlaylistDetails__trackCount, [data-track-count]")
        .map(element => [element.getAttribute("data-track-count"), element.title, text(element)])
        .flat().find(value => /^\s*\d{1,4}(?:\s+(?:tracks?|трек(?:а|ов)?))?\s*$/i.test(value ?? ""));
      const structured = all('script[type="application/ld+json"]').flatMap(script => {
        try {
          const parsed = JSON.parse(script.textContent);
          return parsed["@graph"] ?? parsed;
        } catch { return []; }
      }).find(value => value?.["@type"] === "MusicPlaylist" && Number.isInteger(value.numTracks));
      const count = Number.isInteger(data?.track_count) ? data.track_count
        : structured?.numTracks ?? Number.parseInt(countText, 10);
      return name && Number.isInteger(count) && count >= 0 ? {name, url: expected, count} : null;
    },
  };
})()
