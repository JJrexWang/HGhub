import React, { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { api, platform as envPlatform } from "./api";
import type { DlTask, FavCard, User } from "./types";

export type Platform = "android" | "windows" | "macos";
export type Theme = "dark" | "light";
export type Tab = "home" | "search" | "category" | "profile";

/** 播放页上下文：ids 是来源列表，index 是当前条目。 */
export type FeedContext = {
  kind: "feed";
  key: string;
  videoIds: number[];
  index: number;
  title: string;
  source: string;
  feedKind?: "video" | "post";
  /** 从观看记录续播：起始集数与秒数 */
  startEp?: number;
  startTime?: number;
};

export type Page =
  | FeedContext
  | { kind: "channel"; key: string; cate: string; name: string }
  | { kind: "ranks"; key: string }
  | { kind: "settings"; key: string }
  | { kind: "downloads"; key: string }
  | { kind: "posts"; key: string }
  | { kind: "post"; key: string; id: number };

export type PageInput =
  | { kind: "feed"; videoIds: number[]; index: number; title: string; source?: string; feedKind?: "video" | "post"; startEp?: number; startTime?: number }
  | { kind: "channel"; cate: string; name: string }
  | { kind: "ranks" }
  | { kind: "settings" }
  | { kind: "downloads" }
  | { kind: "posts" }
  | { kind: "post"; id: number };

export type Toast = { id: number; msg: string; icon?: "ok" | "heart" | "star" | "info" };

/** 观看记录：看到第几集、第几秒（`dur` 用来判断是否已看完） */
export type WatchProgress = { id: number; ep: number; t: number; dur: number; at: number };

type Persist = {
  likes: number[];
  favs: number[];
  history: WatchProgress[];
  follows: number[];
  muted: boolean;
  volume: number;
  searchHist: string[];
};

const LS = "hghub_app_v1";
const DEFAULTS: Persist = {
  likes: [], favs: [], history: [], follows: [],
  muted: true, volume: 0.7, searchHist: [],
};

/** 旧版本 history 是 number[]，读出来时升级成带进度的结构 */
function normalizeHistory(raw: unknown): WatchProgress[] {
  if (!Array.isArray(raw)) return [];
  const out: WatchProgress[] = [];
  raw.forEach((item, i) => {
    if (typeof item === "number") {
      out.push({ id: item, ep: 1, t: 0, dur: 0, at: Date.now() - i });
      return;
    }
    const h = item as Partial<WatchProgress> | null;
    if (h && typeof h.id === "number") {
      out.push({ id: h.id, ep: Number(h.ep) || 1, t: Number(h.t) || 0, dur: Number(h.dur) || 0, at: Number(h.at) || Date.now() - i });
    }
  });
  return out;
}

/** 网站收藏接口的返回归一化：兼容 {items:[...]} / 裸数组，
 *  条目兼容 VideoCard 本体、{video_id,...} 与 {video:{...}} 三种形状。 */
function normalizeCollects(res: unknown): { items: FavCard[]; total: number | null } {
  const root = res as Record<string, unknown> | null;
  const rawItems: unknown[] = Array.isArray(res)
    ? res
    : Array.isArray(root?.items)
      ? (root!.items as unknown[])
      : [];
  const pag = (!Array.isArray(res) && root?.pagination) as { total?: unknown } | null;
  const total = pag && typeof pag.total === "number" ? pag.total : null;
  const items: FavCard[] = [];
  for (const raw of rawItems) {
    const r = (raw ?? {}) as Record<string, unknown>;
    const v = (r.video && typeof r.video === "object" ? r.video : r) as Record<string, unknown>;
    const id = Number(v.id ?? r.video_id ?? r.id);
    if (!Number.isFinite(id) || id <= 0) continue;
    items.push({
      id,
      title: typeof v.title === "string" ? v.title : "",
      cover: typeof v.cover === "string" ? v.cover : "",
    });
  }
  return { items, total };
}

function loadPersist(): Persist {
  try {
    const raw = localStorage.getItem(LS);
    if (!raw) return DEFAULTS;
    const p = { ...DEFAULTS, ...(JSON.parse(raw) as Partial<Persist>) };
    return { ...p, history: normalizeHistory(p.history) };
  } catch {
    return DEFAULTS;
  }
}

function readTheme(): Theme {
  try {
    return localStorage.getItem("hghub_theme") === "light" ? "light" : "dark";
  } catch {
    return "dark";
  }
}

function applyTheme(theme: Theme) {
  document.documentElement.classList.toggle("theme-light", theme === "light");
  document.documentElement.style.colorScheme = theme;
}

type Store = {
  platform: Platform;
  theme: Theme; setTheme: (t: Theme) => void;
  tab: Tab; setTab: (t: Tab) => void;
  stack: Page[]; push: (p: PageInput) => void; pop: () => void;
  user: User | null; setUser: (u: User | null) => void;
  loginOpen: boolean; setLoginOpen: (b: boolean) => void;
  logout: () => Promise<void>;
  likes: number[]; favs: number[]; history: WatchProgress[]; follows: number[];
  /** 网站收藏的卡片快照（登录时同步下来，供「我的收藏」直接渲染） */
  favCards: FavCard[];
  /** 网站收藏是否已同步成功（登录且为 true 时 favs 以网站为准） */
  favsSynced: boolean;
  /** 重新从网站拉取收藏（登录状态下） */
  syncFavs: () => Promise<void>;
  isLiked: (id: number) => boolean; isFav: (id: number) => boolean;
  toggleLikeLocal: (id: number) => boolean;
  toggleFavLocal: (id: number) => boolean;
  /** 记一条观看记录；`ep/t/dur` 省略时保留原进度，`ep` 变了就把进度归零 */
  addHistory: (id: number, ep?: number, t?: number, dur?: number) => void;
  clearHistory: () => void;
  toggleFollowLocal: (uid: number) => boolean;
  theater: boolean; setTheater: (v: boolean | ((p: boolean) => boolean)) => void;
  fs: boolean; setFs: (v: boolean | ((p: boolean) => boolean)) => void;
  muted: boolean; setMuted: (v: boolean | ((p: boolean) => boolean)) => void;
  volume: number; setVolume: (v: number) => void;
  searchHist: string[]; addSearch: (q: string) => void; clearSearchHist: () => void;
  toasts: Toast[]; toast: (msg: string, icon?: Toast["icon"]) => void;
  /* 下载（状态在 Rust 侧，这里是镜像：启动拉一次 + download-changed 事件增量替换） */
  downloads: DlTask[];
  refreshDownloads: () => Promise<void>;
  dlOf: (videoId: number, ep: number) => DlTask | undefined;
};

const Ctx = createContext<Store>(null as unknown as Store);
export const useStore = () => useContext(Ctx);
let seq = 0;

export function AppProvider({ children }: { children: React.ReactNode }) {
  const [p, setP] = useState<Persist>(loadPersist);
  const [theme, setThemeState] = useState<Theme>(readTheme);
  const [tab, setTabRaw] = useState<Tab>("home");
  const [stack, setStack] = useState<Page[]>([]);
  const [user, setUser] = useState<User | null>(null);
  const [loginOpen, setLoginOpen] = useState(false);
  const [toasts, setToasts] = useState<Toast[]>([]);
  const [downloads, setDownloads] = useState<DlTask[]>([]);
  /* 网站收藏镜像：登录后从网站同步（remoteFavs 非 null 时 favs 以它为准）；
     退出登录即清空，收藏回落到本地 p.favs */
  const [remoteFavs, setRemoteFavs] = useState<number[] | null>(null);
  const [favCards, setFavCards] = useState<FavCard[]>([]);
  const [favsSynced, setFavsSynced] = useState(false);
  const userRef = useRef<User | null>(null);
  const pRef = useRef(p);
  const remoteFavsRef = useRef<number[] | null>(null);

  useEffect(() => { userRef.current = user; }, [user]);
  useEffect(() => { pRef.current = p; }, [p]);
  const [theater, setTheaterState] = useState(false);
  const [fs, setFsState] = useState(false);
  const toastSeq = useRef(0);

  useEffect(() => {
    applyTheme(theme);
    try { localStorage.setItem("hghub_theme", theme); } catch { /* ignore */ }
  }, [theme]);

  useEffect(() => {
    try { localStorage.setItem(LS, JSON.stringify(p)); } catch { /* ignore */ }
  }, [p]);

  useEffect(() => {
    api.boot()
      .then((r) => { if (r.user && (r.user as User).id) setUser(r.user as User); })
      .catch(() => { /* 离线也能用 */ });
  }, []);

  const refreshDownloads = useCallback(async () => {
    try { setDownloads(await api.dlList()); } catch { /* 下载模块未就绪时忽略 */ }
  }, []);

  /** 从网站拉取当前账号的全部收藏（分页聚合），成功后 favs 以网站为准。 */
  const syncFavs = useCallback(async () => {
    if (!userRef.current) return;
    try {
      const ids: number[] = [];
      const cards: FavCard[] = [];
      const seen = new Set<number>();
      let total = Number.POSITIVE_INFINITY;
      for (let page = 1; page <= 20 && ids.length < total; page++) {
        const res = await api.collects(page, 50);
        const { items, total: t } = normalizeCollects(res);
        if (t != null) total = t;
        if (!items.length) break;
        for (const c of items) {
          if (seen.has(c.id)) continue;
          seen.add(c.id);
          ids.push(c.id);
          cards.push(c);
        }
      }
      remoteFavsRef.current = ids;
      setRemoteFavs(ids);
      setFavCards(cards);
      setFavsSynced(true);
    } catch {
      /* 同步失败不清空既有镜像；从未成功时 favs 回落本地，Profile 里可重试 */
      setFavsSynced(false);
    }
  }, []);

  /* 登录态变化：登录（或启动恢复会话）后同步网站收藏；退出后清空镜像回落本地 */
  const uid = user?.id;
  useEffect(() => {
    if (uid) {
      void syncFavs();
    } else {
      remoteFavsRef.current = null;
      setRemoteFavs(null);
      setFavCards([]);
      setFavsSynced(false);
    }
  }, [uid, syncFavs]);

  useEffect(() => {
    refreshDownloads();
    let un: (() => void) | undefined;
    listen<DlTask[]>("download-changed", (e) => setDownloads(e.payload || []))
      .then((f) => { un = f; })
      .catch(() => {});
    return () => { un?.(); };
  }, [refreshDownloads]);

  const toast = useCallback((msg: string, icon?: Toast["icon"]) => {
    const id = ++toastSeq.current;
    setToasts((t) => [...t.slice(-2), { id, msg, icon }]);
    window.setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), 2200);
  }, []);

  const setTab = useCallback((t: Tab) => { setTabRaw(t); setStack([]); }, []);
  const push = useCallback((pg: PageInput) => {
    setStack((s) => [...s, { ...pg, key: `p${++seq}` } as Page]);
  }, []);
  const pop = useCallback(() => setStack((s) => s.slice(0, -1)), []);

  const toggleLikeLocal = useCallback((id: number) => {
    let on = false;
    setP((s) => {
      on = !s.likes.includes(id);
      return { ...s, likes: on ? [id, ...s.likes] : s.likes.filter((x) => x !== id) };
    });
    return on;
  }, []);
  const toggleFavLocal = useCallback((id: number) => {
    /* 已登录：改网站收藏镜像（Feed 会同时写网站接口）；未登录：改本地收藏 */
    if (userRef.current) {
      const base = remoteFavsRef.current ?? pRef.current.favs;
      const on = !base.includes(id);
      const next = on ? [id, ...base] : base.filter((x) => x !== id);
      remoteFavsRef.current = next;
      setRemoteFavs(next);
      return on;
    }
    let on = false;
    setP((s) => {
      on = !s.favs.includes(id);
      return { ...s, favs: on ? [id, ...s.favs] : s.favs.filter((x) => x !== id) };
    });
    return on;
  }, []);
  const toggleFollowLocal = useCallback((uid: number) => {
    let on = false;
    setP((s) => {
      on = !s.follows.includes(uid);
      return { ...s, follows: on ? [uid, ...s.follows] : s.follows.filter((x) => x !== uid) };
    });
    return on;
  }, []);
  const addHistory = useCallback((id: number, ep?: number, t?: number, dur?: number) => {
    if (!id) return;
    setP((s) => {
      const prev = s.history.find((x) => x.id === id);
      const epChanged = ep !== undefined && prev !== undefined && prev.ep !== ep;
      const entry: WatchProgress = {
        id,
        ep: ep ?? prev?.ep ?? 1,
        t: epChanged ? 0 : (t ?? prev?.t ?? 0),
        dur: dur ?? prev?.dur ?? 0,
        at: Date.now(),
      };
      return { ...s, history: [entry, ...s.history.filter((x) => x.id !== id)].slice(0, 80) };
    });
  }, []);
  const clearHistory = useCallback(() => setP((s) => ({ ...s, history: [] })), []);
  const addSearch = useCallback((q: string) => {
    const t = q.trim();
    if (!t) return;
    setP((s) => ({ ...s, searchHist: [t, ...s.searchHist.filter((x) => x !== t)].slice(0, 12) }));
  }, []);
  const clearSearchHist = useCallback(() => setP((s) => ({ ...s, searchHist: [] })), []);

  const logout = useCallback(async () => {
    try { await api.logout(); } catch { /* ignore */ }
    setUser(null);
    toast("已退出登录", "info");
  }, [toast]);

  const setTheater = useCallback((v: boolean | ((prev: boolean) => boolean)) => {
    setTheaterState((prev) => (typeof v === "function" ? (v as (p: boolean) => boolean)(prev) : v));
  }, []);

  const setFs = useCallback((v: boolean | ((prev: boolean) => boolean)) => {
    setFsState((prev) => (typeof v === "function" ? (v as (p: boolean) => boolean)(prev) : v));
  }, []);

  const setMuted = useCallback((v: boolean | ((prev: boolean) => boolean)) => {
    setP((s) => ({ ...s, muted: typeof v === "function" ? v(s.muted) : v }));
  }, []);
  const setVolume = useCallback((v: number) => {
    const volume = Math.max(0, Math.min(1, v));
    setP((s) => ({ ...s, volume, muted: volume === 0 }));
  }, []);

  /* 对外暴露的收藏：已登录且网站镜像就绪时以网站为准，否则本地收藏 */
  const effFavs = user && remoteFavs ? remoteFavs : p.favs;

  const value = useMemo<Store>(() => ({
    platform: envPlatform,
    theme, setTheme: setThemeState,
    tab, setTab, stack, push, pop,
    user, setUser, loginOpen, setLoginOpen, logout,
    likes: p.likes, favs: effFavs, history: p.history, follows: p.follows,
    favCards, favsSynced, syncFavs,
    isLiked: (id) => p.likes.includes(id),
    isFav: (id) => effFavs.includes(id),
    toggleLikeLocal, toggleFavLocal, toggleFollowLocal, addHistory, clearHistory,
    muted: p.muted, setMuted, volume: p.volume, setVolume, theater, setTheater, fs, setFs,
    searchHist: p.searchHist, addSearch, clearSearchHist,
    toasts, toast,
    downloads, refreshDownloads,
    dlOf: (videoId: number, ep: number) => downloads.find((t) => t.video_id === videoId && t.ep === ep),
  }), [theme, tab, setTab, stack, push, pop, user, loginOpen, logout,
    p, effFavs, remoteFavs, favCards, favsSynced, syncFavs,
    toggleLikeLocal, toggleFavLocal, toggleFollowLocal, addHistory, clearHistory,
    setMuted, setVolume, addSearch, clearSearchHist, toasts, toast, theater, setTheater, fs, setFs,
    downloads, refreshDownloads]);

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}
