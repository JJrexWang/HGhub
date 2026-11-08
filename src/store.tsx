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
  /** 已下载作品的线上集数快照（启动时逐部查询刷新）：videoId → 线上集号列表。
   *  与本地任务一对比就知道哪部有新集（徽标与「一键更新库存」都靠它）。 */
  dlSnap: Record<number, number[]>;
};

const LS = "hghub_app_v1";
const DEFAULTS: Persist = {
  likes: [], favs: [], history: [], follows: [],
  muted: true, volume: 0.7, searchHist: [], dlSnap: {},
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

/** 收藏条目里可能指向视频的 id 来源（同一接口内形状统一，定下命中来源即可全量套用） */
type IdSrc = "video_id" | "nested" | "own";
type CollectItem = { cands: { src: IdSrc; id: number }[]; title: string; cover: string };

/** 解析一条网站收藏。关键：这家 API 的约定是 `video_id` 才是指向视频的字段，
 *  条目自身的 `id` 是「收藏记录」的 id——热榜、帖子条目同理。拿记录 id 去取详情/播放
 *  会报「视频不存在」：网格的标题封面都取自记录本身，所以列表能显示、点开却播不了。
 *  嵌套 `{video:{...}}` 形状则以 video 对象为准；标题/封面再兜底 video_title/video_cover。 */
function parseCollectItem(raw: unknown): CollectItem | null {
  const r = (raw ?? {}) as Record<string, unknown>;
  const nested = r.video && typeof r.video === "object" ? (r.video as Record<string, unknown>) : null;
  const cands: { src: IdSrc; id: number }[] = [];
  const add = (src: IdSrc, v: unknown) => {
    const n = Number(v);
    if (Number.isFinite(n) && n > 0 && !cands.some((c) => c.id === n)) cands.push({ src, id: n });
  };
  add("video_id", r.video_id ?? r.target_id);
  if (nested) add("nested", nested.id);
  add("own", r.id);
  if (!cands.length) return null;
  const str = (...vs: unknown[]) => {
    for (const v of vs) if (typeof v === "string" && v) return v;
    return "";
  };
  return {
    cands,
    title: str(nested?.title, r.video_title, r.title, r.name),
    cover: str(nested?.cover, r.video_cover, r.cover, r.poster),
  };
}

/** 网站收藏接口的一页：兼容 {items:[...]} / 裸数组两种信封。 */
function parseCollectsPage(res: unknown): { items: CollectItem[]; total: number | null } {
  const root = res as Record<string, unknown> | null;
  const rawItems: unknown[] = Array.isArray(res)
    ? res
    : Array.isArray(root?.items)
      ? (root!.items as unknown[])
      : [];
  const pag = (!Array.isArray(res) && root?.pagination) as { total?: unknown } | null;
  const total = pag && typeof pag.total === "number" ? pag.total : null;
  return { items: rawItems.map(parseCollectItem).filter((x): x is CollectItem => !!x), total };
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
  /** 已下载作品的线上集数快照：videoId → 线上集号列表（启动时自动查一次） */
  dlSnap: Record<number, number[]>;
  /** 某作品比本地已下载进度更新的新集集号（快照里超出本地最大已持有集号的部分） */
  dlNewEps: (videoId: number) => number[];
  /** 正在逐部查询已下载作品的线上更新 */
  dlChecking: boolean;
  /** 重新查询全部已下载作品的线上集数（更新快照），返回最新快照 */
  checkDlUpdates: () => Promise<Record<number, number[]>>;
  /** 批量操作进度（收藏全量下载 / 一键更新库存期间非 null） */
  bulk: { kind: "favs" | "updates"; done: number; total: number } | null;
  /** 一键下载/更新全部收藏：全部作品全部集数入队，已下载/下载中的集自动跳过 */
  downloadAllFavs: () => Promise<void>;
  /** 一键更新库存：先刷新快照，再把已下载作品的新集全部入队 */
  updateDownloaded: () => Promise<void>;
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
  const downloadsRef = useRef<DlTask[]>([]);
  const [dlChecking, setDlChecking] = useState(false);
  const dlCheckingRef = useRef(false);
  const [bulk, setBulk] = useState<{ kind: "favs" | "updates"; done: number; total: number } | null>(null);
  const bulkRef = useRef(false);
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
  useEffect(() => { downloadsRef.current = downloads; }, [downloads]);
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
    try {
      const list = await api.dlList();
      downloadsRef.current = list;
      setDownloads(list);
    } catch { /* 下载模块未就绪时忽略 */ }
  }, []);

  /** 从网站拉取当前账号的全部收藏（分页聚合），成功后 favs 以网站为准。 */
  const syncFavs = useCallback(async () => {
    if (!userRef.current) return;
    try {
      const parsed: CollectItem[] = [];
      const seenKey = new Set<string>();
      let total = Number.POSITIVE_INFINITY;
      for (let page = 1; page <= 20 && parsed.length < total; page++) {
        const res = await api.collects(page, 50);
        const { items, total: t } = parseCollectsPage(res);
        if (t != null) total = t;
        if (!items.length) break;
        for (const it of items) {
          const key = it.cands.map((c) => c.id).join("/");
          if (seenKey.has(key)) continue;
          seenKey.add(key);
          parsed.push(it);
        }
      }
      /* 定每条的视频 id，三层保险：
         1) 候选命中账号的 favorite_video_ids（/me 给的真视频 id 集合）者优先；
         2) 集合拿不到且条目有多个候选时，用第一条探一次详情接口——能拉到且标题对得上
            才算命中，把命中来源套用到全部条目（同一接口形状统一，只探一次）；
         3) 都没有就按候选默认顺序（video_id → 嵌套 video → 记录自身 id）。 */
      const favSet = new Set(userRef.current?.interactions?.favorite_video_ids ?? []);
      let winSrc: IdSrc | null = null;
      if (!favSet.size) {
        const probe = parsed.find((it) => it.cands.length > 1);
        if (probe) {
          const norm = (s: string) => s.replace(/\s+/g, "");
          for (const cand of probe.cands) {
            try {
              const d = await api.video(cand.id);
              const dt = typeof d?.title === "string" ? norm(d.title) : "";
              if (d && (!probe.title || !dt || dt === norm(probe.title) || dt.includes(norm(probe.title)) || norm(probe.title).includes(dt))) {
                winSrc = cand.src;
                break;
              }
            } catch { /* 该候选不是视频 id，试下一个 */ }
          }
        }
      }
      const ids: number[] = [];
      const cards: FavCard[] = [];
      const seen = new Set<number>();
      for (const it of parsed) {
        const hit = favSet.size ? it.cands.find((c) => favSet.has(c.id)) : undefined;
        const bySrc = winSrc ? it.cands.find((c) => c.src === winSrc) : undefined;
        const id = (hit ?? bySrc ?? it.cands[0]).id;
        if (seen.has(id)) continue;
        seen.add(id);
        ids.push(id);
        cards.push({ id, title: it.title, cover: it.cover });
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

  /* ── 已下载作品的线上更新 ───────────────────────────────────
     「算已有」的集：已完成 + 排队/下载中/暂停。失败/取消不算已有，
     一键操作时会经 Rust 侧 enqueue 语义自动重新入队续传。 */
  const HAVE_ST = new Set(["done", "queued", "downloading", "paused"]);
  const newEpsOf = (tasks: DlTask[], snap: Record<number, number[]>, id: number): number[] => {
    const remote = snap[id];
    if (!remote?.length) return [];
    let localMax = 0;
    for (const t of tasks) if (t.video_id === id && HAVE_ST.has(t.status)) localMax = Math.max(localMax, t.ep);
    return remote.filter((ep) => ep > localMax);
  };

  /** 逐部查询已下载作品的线上集数并刷新快照（4 部一批，最多 80 部）。返回最新快照。 */
  const checkDlUpdates = useCallback(async (): Promise<Record<number, number[]>> => {
    if (dlCheckingRef.current) return pRef.current.dlSnap;
    const works = [...new Set(downloadsRef.current.map((t) => t.video_id))].slice(0, 80);
    if (!works.length) return pRef.current.dlSnap;
    dlCheckingRef.current = true;
    setDlChecking(true);
    const snap: Record<number, number[]> = { ...pRef.current.dlSnap };
    try {
      for (let i = 0; i < works.length; i += 4) {
        const batch = works.slice(i, i + 4);
        const rs = await Promise.allSettled(batch.map((id) => api.video(id)));
        rs.forEach((r, j) => {
          if (r.status !== "fulfilled") return;
          const eps = [...new Set((r.value.episodes || []).map((e) => e.ep_num).filter((n) => n > 0))].sort((a, b) => a - b);
          if (eps.length) snap[batch[j]] = eps;
          else if (r.value.total_episodes > 0) {
            snap[batch[j]] = Array.from({ length: r.value.total_episodes }, (_, k) => k + 1);
          }
        });
      }
      setP((s) => ({ ...s, dlSnap: snap }));
      return snap;
    } finally {
      dlCheckingRef.current = false;
      setDlChecking(false);
    }
  }, []);

  /** 一键下载/更新全部收藏：逐部读线上集数，与本地任务对比后只把缺的集入队。 */
  const downloadAllFavs = useCallback(async () => {
    if (bulkRef.current) return;
    const favIds = userRef.current && remoteFavsRef.current ? remoteFavsRef.current : pRef.current.favs;
    if (!favIds.length) { toast("还没有收藏可下载", "info"); return; }
    bulkRef.current = true;
    setBulk({ kind: "favs", done: 0, total: favIds.length });
    let addWorks = 0, addEps = 0, skipEps = 0, failWorks = 0;
    try {
      await refreshDownloads();
      for (let i = 0; i < favIds.length; i++) {
        const id = favIds[i];
        try {
          const d = await api.video(id);
          let eps = [...new Set((d.episodes || []).map((e) => e.ep_num).filter((n) => n > 0))].sort((a, b) => a - b);
          if (!eps.length && d.total_episodes > 0) eps = Array.from({ length: d.total_episodes }, (_, k) => k + 1);
          const have = new Set(downloadsRef.current.filter((t) => t.video_id === id && HAVE_ST.has(t.status)).map((t) => t.ep));
          const todo = eps.filter((ep) => !have.has(ep));
          skipEps += eps.length - todo.length;
          if (todo.length) {
            await api.dlEnqueueSeries(id, todo);
            addWorks++;
            addEps += todo.length;
            await refreshDownloads();
          }
        } catch { failWorks++; }
        setBulk({ kind: "favs", done: i + 1, total: favIds.length });
      }
      toast(addEps
        ? `收藏下载：新增 ${addWorks} 部、${addEps} 集已入队，跳过已下载 ${skipEps} 集${failWorks ? `，${failWorks} 部读取失败` : ""}`
        : `收藏里的集都已下载，跳过 ${skipEps} 集${failWorks ? `，${failWorks} 部读取失败` : ""}`, "ok");
    } finally {
      bulkRef.current = false;
      setBulk(null);
      void refreshDownloads();
    }
  }, [refreshDownloads, toast]);

  /** 一键更新库存：先刷新线上集数快照，再把每部已下载作品的新集入队。 */
  const updateDownloaded = useCallback(async () => {
    if (bulkRef.current) return;
    if (!downloadsRef.current.length) { toast("还没有已下载的作品", "info"); return; }
    bulkRef.current = true;
    try {
      const snap = await checkDlUpdates();
      const works = [...new Set(downloadsRef.current.map((t) => t.video_id))];
      setBulk({ kind: "updates", done: 0, total: works.length });
      let addWorks = 0, addEps = 0;
      for (let i = 0; i < works.length; i++) {
        const id = works[i];
        const news = newEpsOf(downloadsRef.current, snap, id);
        if (news.length) {
          try {
            await api.dlEnqueueSeries(id, news);
            addWorks++;
            addEps += news.length;
            await refreshDownloads();
          } catch { /* 单部失败不中断其余作品 */ }
        }
        setBulk({ kind: "updates", done: i + 1, total: works.length });
      }
      toast(addEps ? `库存更新：${addWorks} 部新增 ${addEps} 集已入队` : "库存已是最新，没有新集", "ok");
    } finally {
      bulkRef.current = false;
      setBulk(null);
      void refreshDownloads();
    }
  }, [checkDlUpdates, refreshDownloads, toast]);

  /* 每次打开软件：下载列表就绪后，自动查一次已下载作品的线上更新（更新徽标靠它） */
  const dlBootRef = useRef(false);
  useEffect(() => {
    if (dlBootRef.current) return;
    dlBootRef.current = true;
    void refreshDownloads().then(() => checkDlUpdates());
  }, [refreshDownloads, checkDlUpdates]);

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
    dlSnap: p.dlSnap, dlNewEps: (videoId: number) => newEpsOf(downloads, p.dlSnap, videoId),
    dlChecking, checkDlUpdates, bulk, downloadAllFavs, updateDownloaded,
  }), [theme, tab, setTab, stack, push, pop, user, loginOpen, logout,
    p, effFavs, remoteFavs, favCards, favsSynced, syncFavs,
    toggleLikeLocal, toggleFavLocal, toggleFollowLocal, addHistory, clearHistory,
    setMuted, setVolume, addSearch, clearSearchHist, toasts, toast, theater, setTheater, fs, setFs,
    downloads, refreshDownloads, dlChecking, checkDlUpdates, bulk, downloadAllFavs, updateDownloaded]);

  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}
