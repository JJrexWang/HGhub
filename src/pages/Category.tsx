import { useEffect, useRef, useState } from "react";
import { api, fmt } from "../api";
import type { Post, VideoCard } from "../types";
import { useStore } from "../store";
import { Masonry, RowCard } from "../components/cards";
import { Empty, Img } from "../components/ui";
import { IBack, IChevR, IGrid, ITrophy } from "../icons";

/* 频道候选与文案：频道名单以站点 /api/site/categories/video 为准（与官网同步），
   本地表只作接口不可用时的兜底 + 已知频道的展示名/描述覆盖。
   注意：注册表里在列 ≠ 有内容——换脸/魔改的分类接口已回「内容已下线」，
   总览按「实际拉到内容」过滤，下线或为空的频道不展示。 */
export const CHANNELS = [
  { cate: "ai-duanju", name: "AI成人短剧", desc: "高能反转 · 一口气看完" },
  { cate: "ai-manju", name: "AI成人漫剧", desc: "国创漫剧 · 每周更新" },
  { cate: "ai-huanlian", name: "AI换脸", desc: "换脸演绎 · 全新剧情" },
  { cate: "ai-mogai", name: "AI魔改", desc: "魔改名场面 · 脑洞大开" },
] as const;

type Channel = { cate: string; name: string; desc: string };

const POOL = 24; // 每行预拉取上限，实际展示数按容器宽度切片
const CARD_W = 104;
const GAP = 10;

/** 按容器宽度算一行能放下几张卡（分类行与吃瓜行共用同一个页面宽度）。 */
function useFitCount() {
  const ref = useRef<HTMLDivElement>(null);
  const [fit, setFit] = useState(8);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const update = () => {
      const w = el.clientWidth - 32; // 行内 px-4 两侧留白
      setFit(Math.max(3, Math.min(POOL, Math.floor((w + GAP) / (CARD_W + GAP)))));
    };
    update();
    const ro = new ResizeObserver(update);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  return [ref, fit] as const;
}

/* ================= 分类总览 ================= */
export default function Category({ wide }: { wide: boolean }) {
  const { push } = useStore();
  const [channels, setChannels] = useState<Channel[]>([]);
  const [rows, setRows] = useState<Record<string, VideoCard[]>>({});
  const [posts, setPosts] = useState<Post[]>([]);
  const [loading, setLoading] = useState(true);
  const [scrollRef, fit] = useFitCount();

  useEffect(() => {
    let alive = true;
    (async () => {
      // 1) 频道名单与官网同步：优先站点注册表，失败回落本地表
      let list: Channel[] = CHANNELS.map((c) => ({ ...c }));
      try {
        const res = await api.siteCategories();
        const items = (res?.items || []).filter((i) => i.route);
        if (items.length) {
          list = [...items]
            .sort((a, b) => (b.sort || 0) - (a.sort || 0))
            .map((i) => {
              const local = CHANNELS.find((c) => c.cate === i.route);
              return {
                cate: i.route,
                name: local?.name || i.name,
                desc: local?.desc || "精选内容 · 持续上新",
              };
            });
        }
      } catch { /* 用本地兜底表 */ }
      // 2) 逐频道拉首屏：报错（已下线）或空的频道不进总览
      const entries = await Promise.all(
        list.map(async (c) => {
          try {
            const res = await api.category(c.cate, 1, POOL);
            return [c, res?.items || []] as const;
          } catch {
            return [c, [] as VideoCard[]] as const;
          }
        }),
      );
      let p: Post[] = [];
      try { p = (await api.posts(1, POOL))?.items || []; } catch { p = []; }
      if (!alive) return;
      const visible = entries.filter(([, items]) => items.length > 0);
      setChannels(visible.map(([c]) => c));
      setRows(Object.fromEntries(visible.map(([c, items]) => [c.cate, items])));
      setPosts(p);
      setLoading(false);
    })();
    return () => { alive = false; };
  }, []);

  const channelCount = channels.length + (posts.length ? 1 : 0) + 1;

  return (
    <div className="absolute inset-0 bg-bg flex flex-col">
      <div className="shrink-0 px-4 pb-1" style={{ paddingTop: "calc(var(--sat, 0px) + 14px)" }}>
        <div className="flex items-end gap-2.5">
          <span className="text-[24px] font-bold text-on leading-none">分类</span>
          <span className="text-[11px] text-mut pb-0.5">{loading ? "每周持续上新" : `${channelCount} 个频道 · 每周持续上新`}</span>
        </div>
      </div>

      <div ref={scrollRef} className="flex-1 scroll-y pb-24 pt-2">
        {/* 排行榜入口 */}
        <button onClick={() => push({ kind: "ranks" })}
          className="mx-4 mb-5 w-[calc(100%-2rem)] flex items-center gap-3 rounded-(--r-md) bg-s2 p-3.5 text-left pressable elev-1">
          <span className="w-10 h-10 rounded-(--r-sm) bg-pc text-on-pc flex items-center justify-center shrink-0"><ITrophy size={20} /></span>
          <span className="flex-1">
            <span className="block text-[14.5px] font-semibold text-on">排行榜</span>
            <span className="block text-[10.5px] text-mut mt-0.5">热播榜 · 推荐榜 · 潜力榜</span>
          </span>
          <IChevR size={16} className="text-mut" />
        </button>

        {loading && <div className="flex justify-center py-10"><div className="w-8 h-8 spinner" /></div>}

        {!loading && channels.map((c, ci) => {
          const list = (rows[c.cate] || []).slice(0, fit);
          return (
            <section key={c.cate} className="mb-5 a-rise" style={{ animationDelay: `${Math.min(ci * 50, 300)}ms` }}>
              <button onClick={() => push({ kind: "channel", cate: c.cate, name: c.name })}
                className="w-full flex items-center gap-2.5 px-4 mb-2.5 text-left pressable group">
                <span className="w-[36px] h-[36px] rounded-(--r-sm) tonal flex items-center justify-center shrink-0 text-primary">
                  <IGrid size={19} strokeWidth={1.9} />
                </span>
                <span className="flex-1 min-w-0">
                  <span className="block text-[15.5px] font-semibold text-on leading-tight">{c.name}</span>
                  <span className="block text-[10.5px] text-mut mt-0.5 truncate">{c.desc}</span>
                </span>
                <span className="flex items-center gap-0.5 text-[11.5px] text-mut group-hover:text-primary transition-colors">
                  全部<IChevR size={14} />
                </span>
              </button>
              <div className="scroll-x flex gap-2.5 px-4 snap-x">
                {list.map((v) => (
                  <RowCard key={v.id} v={v} onOpen={() => push({ kind: "feed", videoIds: list.map((x) => x.id), index: list.findIndex((x) => x.id === v.id), title: c.name })} />
                ))}
              </div>
            </section>
          );
        })}

        {/* 吃瓜：有内容才展示 */}
        {!loading && posts.length > 0 && (
        <section className="mb-5 a-rise">
          <button onClick={() => push({ kind: "posts" })}
            className="w-full flex items-center gap-2.5 px-4 mb-2.5 text-left pressable group">
            <span className="w-[36px] h-[36px] rounded-(--r-sm) tonal flex items-center justify-center shrink-0 text-primary">
              <IGrid size={19} strokeWidth={1.9} />
            </span>
            <span className="flex-1 min-w-0">
              <span className="block text-[15.5px] font-semibold text-on leading-tight">黄果吃瓜</span>
              <span className="block text-[10.5px] text-mut mt-0.5 truncate">圈内爆料 · 每日更新</span>
            </span>
            <span className="flex items-center gap-0.5 text-[11.5px] text-mut group-hover:text-primary transition-colors">
              全部<IChevR size={14} />
            </span>
          </button>
          <div className="scroll-x flex gap-2.5 px-4 snap-x">
            {posts.slice(0, fit).map((post) => (
              <button key={post.id} onClick={() => push({ kind: "post", id: post.id })}
                className="w-[104px] shrink-0 text-left snap-start pressable">
                <div className="relative w-full rounded-(--r-sm) overflow-hidden bg-s1 elev-1" style={{ paddingTop: "133%" }}>
                  <Img src={post.cover} />
                  <div className="absolute inset-x-0 bottom-0 h-1/2 bg-gradient-to-t from-black/75 to-transparent" />
                  <span className="absolute bottom-1.5 right-1.5 text-[9px] font-semibold text-white/90 tsh">{fmt(post.hot)}</span>
                </div>
                <div className="mt-1.5 text-[11.5px] font-medium text-on clamp-1 leading-tight">{post.title}</div>
              </button>
            ))}
          </div>
        </section>
        )}

        {!loading && <div className="px-4 text-center text-[10.5px] text-dim pt-1">— 已展示全部频道 —</div>}
      </div>
      {wide && <div />}
    </div>
  );
}

/* ================= 频道详情页 ================= */
export function ChannelPage({ cate, name, onBack, wide }: { cate: string; name: string; onBack: () => void; wide: boolean }) {
  const { push } = useStore();
  const [sort, setSort] = useState<"最新" | "最热" | "高分">("最新");
  const [items, setItems] = useState<VideoCard[]>([]);
  const [page, setPage] = useState(0);
  const [pages, setPages] = useState(1);
  const [loading, setLoading] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const busy = useRef(false);
  const sentinel = useRef<HTMLDivElement>(null);

  const loadMore = async () => {
    if (busy.current || page >= pages) return;
    busy.current = true;
    setLoading(true);
    try {
      const next = page + 1;
      const res = await api.category(cate, next, 20);
      setItems((prev) => {
        const seen = new Set(prev.map((x) => x.id));
        return [...prev, ...(res?.items || []).filter((x) => !seen.has(x.id))];
      });
      setPage(next);
      setPages(res?.pagination?.pages || next);
      setErr(null);
    } catch (e) {
      setErr((e as Error).message || "加载失败");
    } finally {
      busy.current = false;
      setLoading(false);
    }
  };

  useEffect(() => { loadMore(); /* eslint-disable-next-line */ }, []);
  useEffect(() => {
    const el = sentinel.current;
    if (!el) return;
    const io = new IntersectionObserver((es) => { if (es[0].isIntersecting) loadMore(); }, { rootMargin: "600px" });
    io.observe(el);
    return () => io.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [page, pages]);

  const sorted = [...items].sort((a, b) => {
    if (sort === "最热") return (b.hot || 0) - (a.hot || 0);
    if (sort === "高分") return (b.score || 0) - (a.score || 0);
    return b.id - a.id;
  });

  return (
    <div className="absolute inset-0 bg-bg flex flex-col">
      <div className="shrink-0 bg-s1" style={{ paddingTop: "var(--sat, 0px)" }}>
        <div className="px-3 pt-2 pb-3 flex items-center gap-2.5">
          <button onClick={onBack} className="w-10 h-10 rounded-full flex items-center justify-center text-on hover:bg-s3 pressable"><IBack size={20} /></button>
          <span className="w-[40px] h-[40px] rounded-(--r-sm) bg-pc text-on-pc flex items-center justify-center"><IGrid size={21} strokeWidth={1.9} /></span>
          <div className="flex-1 min-w-0">
            <div className="text-[17px] font-bold text-on leading-tight">{name}</div>
            <div className="text-[10.5px] text-mut">共 {items.length} 部 · 已加载 {page}/{Math.max(pages, 1)} 页</div>
          </div>
        </div>
        <div className="px-4 pb-3 flex gap-2">
          {(["最新", "最热", "高分"] as const).map((s) => (
            <button key={s} onClick={() => setSort(s)}
              className={`px-4 h-8 rounded-(--r-ctl) text-[12px] font-semibold pressable ${sort === s ? "tonal text-on-sec" : "bg-s3 text-mut"}`}
              style={sort === s ? { background: "var(--p-sec)", color: "var(--p-primary)" } : undefined}>{s}</button>
          ))}
        </div>
      </div>
      <div className="flex-1 scroll-y px-4 pt-3 pb-8">
        {sorted.length ? (
          <Masonry videos={sorted} wide={wide} onOpen={(v) => push({ kind: "feed", videoIds: sorted.map((x) => x.id), index: sorted.findIndex((x) => x.id === v.id), title: name })} />
        ) : err ? (
          <Empty icon={<IGrid size={26} />} title="加载失败" sub={err} action="重试" onAction={loadMore} />
        ) : (
          <div className="flex justify-center py-10"><div className="w-8 h-8 spinner" /></div>
        )}
        <div ref={sentinel} className="h-10" />
        {loading && <div className="flex justify-center py-3"><div className="w-6 h-6 spinner" /></div>}
        {!loading && page >= pages && sorted.length > 0 && (
          <div className="text-center text-[10.5px] text-dim py-3">— 已经到底啦 —</div>
        )}
      </div>
    </div>
  );
}