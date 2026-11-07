import { useEffect, useRef, useState } from "react";
import { api, fmt } from "../api";
import type { Comment, CommentPage, Episode } from "../types";
import { useStore } from "../store";
import { Avatar, Empty } from "./ui";
import { ICheck, IClose, IDownload, IHeart, ISendF } from "../icons";

/* ================= 选集 ================= */
export function EpisodesPanel({ videoId, episodes, current, onPick, onClose, bare = false }: {
  videoId: number; episodes: Episode[]; current: number; onPick: (ep: number) => void; onClose: () => void;
  /** 嵌在双栏抽屉里时，隐藏自带的标题栏与底注 */
  bare?: boolean;
}) {
  const [asc, setAsc] = useState(true);
  const { dlOf, toast, push } = useStore();
  const list = asc ? episodes : [...episodes].reverse();

  const dlSeries = () => {
    api.dlEnqueueSeries(videoId)
      .then((ts) => toast(`已加入 ${ts.length} 集到下载队列（顺序下载）`, "ok"))
      .catch((e) => toast((e as Error).message || "整部下载入队失败", "info"));
  };
  const dlOne = (ep: number) => {
    const t = dlOf(videoId, ep);
    if (!t) {
      api.dlEnqueue(videoId, ep).then(() => toast(`第 ${ep} 集已加入下载队列`, "ok")).catch((e) => toast((e as Error).message || "入队失败", "info"));
      return;
    }
    if (t.status === "paused" || t.status === "failed" || t.status === "canceled") {
      api.dlResume(t.id).then(() => toast("已继续下载", "ok")).catch(() => {});
    } else if (t.status !== "done") {
      push({ kind: "downloads" });
    }
  };

  /** 每集的下载状态点：绿=已下载，蓝脉冲=下载中，灰=排队/暂停，红=失败 */
  const dot = (ep: number) => {
    const t = dlOf(videoId, ep);
    if (!t) return null;
    const color = t.status === "done" ? "#2ea45a"
      : t.status === "downloading" ? "#4c8dff"
      : t.status === "failed" ? "#e5534b" : "rgba(140,140,140,.9)";
    return <span className="absolute top-1 right-1 w-1.5 h-1.5 rounded-full" style={{ background: color }} />;
  };

  return (
    <div className="flex flex-col h-full">
      {bare ? (
        <div className="shrink-0 flex items-center gap-2 px-4 pb-2">
          <span className="text-[10.5px] text-mut">共 {episodes.length} 集</span>
          <button onClick={dlSeries} className="ml-auto flex items-center gap-1 text-[11px] text-primary pressable">
            <IDownload size={12} />整部下载
          </button>
          <button onClick={() => setAsc((v) => !v)} className="text-[11px] text-primary pressable">{asc ? "倒序" : "正序"}</button>
        </div>
      ) : (
        <div className="shrink-0 flex items-center gap-2 px-4 h-12">
          <span className="text-[13.5px] font-bold text-on">选集</span>
          <span className="text-[10.5px] text-mut">共 {episodes.length} 集</span>
          <button onClick={dlSeries} className="ml-auto flex items-center gap-1 text-[11px] text-primary pressable">
            <IDownload size={12} />整部下载
          </button>
          <button onClick={() => setAsc((v) => !v)} className="text-[11px] text-primary pressable">{asc ? "倒序" : "正序"}</button>
          <button onClick={onClose} aria-label="关闭" className="w-7 h-7 rounded-full bg-s3 flex items-center justify-center text-mut pressable">
            <IClose size={13} />
          </button>
        </div>
      )}
      <div className="flex-1 scroll-y px-4 pb-4">
        <div className="grid grid-cols-4 gap-2">
          {list.map((e) => (
            <button key={e.ep_num} onClick={() => onPick(e.ep_num)}
              className={`relative h-10 rounded-(--r-sm) text-[12px] font-semibold pressable ${e.ep_num === current ? "bg-primary text-on-primary" : "bg-s3 text-on/80"}`}>
              {e.ep_num}
              {dot(e.ep_num)}
            </button>
          ))}
        </div>
        {!episodes.length && <div className="text-center text-[11.5px] text-dim py-8">暂无剧集</div>}
        <div className="text-[10px] text-dim mt-3 leading-relaxed">圆点：绿=已下载 · 蓝=下载中 · 灰=排队/暂停 · 红=失败。长按一集的数字格没有菜单，用下方按钮下载当前集。</div>
      </div>
      {!bare && (
        <div className="shrink-0 px-4 py-2.5 bg-s2/60 flex items-center gap-2">
          <span className="text-[10.5px] text-dim">第 {current} 集</span>
          <button onClick={() => dlOne(current)} className="ml-auto flex items-center gap-1.5 h-8 px-3 rounded-(--r-ctl) bg-s3 text-[11.5px] text-on pressable">
            {dlOf(videoId, current)?.status === "done" ? <><ICheck size={13} className="text-mint" />已下载</> : <><IDownload size={13} />下载本集</>}
          </button>
          <button onClick={() => push({ kind: "downloads" })} className="h-8 px-3 rounded-(--r-ctl) bg-s3 text-[11.5px] text-on pressable">下载管理</button>
        </div>
      )}
    </div>
  );
}

/* ================= 评论 ================= */
export function CommentsPanel({ videoId, ep, total, kind = "video", onClose }: {
  videoId: number; ep: number; total: number; kind?: "video" | "post"; onClose: () => void;
}) {
  const { user, setLoginOpen, toast } = useStore();
  const [items, setItems] = useState<Comment[]>([]);
  const [page, setPage] = useState(0);
  const [pages, setPages] = useState(1);
  const [loading, setLoading] = useState(false);
  const [text, setText] = useState("");
  const [sending, setSending] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  const load = async (p: number) => {
    setLoading(true);
    try {
      const res: CommentPage | null = kind === "post"
        ? await api.postComments(videoId, p, 20)
        : await api.comments(videoId, ep, p, 20);
      setItems((prev) => {
        const base = p === 1 ? [] : prev;
        const seen = new Set(base.map((x) => x.id));
        return [...base, ...(res?.items || []).filter((x) => !seen.has(x.id))];
      });
      setPage(p);
      setPages(res?.pagination?.pages || 1);
    } catch {
      if (p === 1) setItems([]);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => { load(1); /* eslint-disable-next-line */ }, [videoId, ep, kind]);

  const send = async () => {
    const t = text.trim();
    if (!t) return;
    if (!user) { setLoginOpen(true); return; }
    setSending(true);
    try {
      await api.commentSend(videoId, ep, t);
      setText("");
      toast("评论已提交，审核通过后展示", "ok");
      load(1);
    } catch (e) {
      toast((e as Error).message || "评论失败", "info");
    } finally {
      setSending(false);
    }
  };

  return (
    <div className="flex flex-col h-full">
      <div className="shrink-0 flex items-center gap-2 px-4 h-12">
        <span className="text-[13.5px] font-bold text-on">评论</span>
        <span className="text-[10.5px] text-mut">{fmt(total)} · 第 {ep} 集</span>
        <button onClick={onClose} aria-label="关闭" className="ml-auto w-7 h-7 rounded-full bg-s3 flex items-center justify-center text-mut pressable">
          <IClose size={13} />
        </button>
      </div>

      <div className="flex-1 scroll-y px-4">
        {items.length ? items.map((c) => (
          <div key={c.id} className="flex gap-2.5 py-3 border-b border-s3 last:border-0">
            <Avatar size={32} name={c.nickname} hue={(c.user_id * 37) % 360} />
            <div className="flex-1 min-w-0">
              <div className="flex items-center gap-1.5">
                <span className="text-[11.5px] font-semibold text-on/90">{c.nickname}</span>
                <span className="text-[10px] text-dim">{c.time}</span>
              </div>
              <p className="text-[12.5px] text-on/80 mt-1 leading-relaxed whitespace-pre-wrap">{c.text}</p>
              <div className="flex items-center gap-3 mt-1.5 text-[10.5px] text-dim">
                <span className="flex items-center gap-1"><IHeart size={11} />{c.like_count || 0}</span>
                {c.reply_count > 0 && <span>{c.reply_count} 条回复</span>}
              </div>
            </div>
          </div>
        )) : loading ? (
          <div className="flex justify-center py-10"><div className="w-7 h-7 spinner" /></div>
        ) : (
          <Empty icon={<IHeart size={22} />} title="还没有评论" sub="来抢第一个沙发" />
        )}
        {items.length > 0 && (
          <div className="flex justify-center py-4">
            {loading ? <div className="w-5 h-5 spinner" />
              : page < pages ? (
                <button onClick={() => load(page + 1)} className="px-4 h-8 rounded-(--r-ctl) bg-s3 text-[11.5px] text-on/80 pressable">查看更多</button>
              ) : <span className="text-[10.5px] text-dim">— 到底啦 —</span>}
          </div>
        )}
      </div>

      <form className="shrink-0 flex items-center gap-2 px-3 py-2.5 bg-s2/70" onSubmit={(e) => { e.preventDefault(); send(); }}>
        <Avatar size={30} name={user?.nickname || "我"} hue={322} />
        <input ref={inputRef} value={text} onChange={(e) => setText(e.target.value)}
          onFocus={() => { if (!user) setLoginOpen(true); }}
          placeholder={user ? "说点什么…" : "登录后参与评论"}
          className="flex-1 min-w-0 h-9 rounded-(--r-ctl) bg-s4 px-3.5 text-[12.5px] text-on placeholder-dim outline-none focus:bg-s3" />
        <button type="submit" disabled={!text.trim() || sending}
          className={`w-9 h-9 rounded-full flex items-center justify-center pressable ${text.trim() ? "brand-grad glow-neon" : "bg-s3 text-dim"}`}>
          {sending ? <span className="w-4 h-4 spinner" /> : <ISendF size={16} />}
        </button>
      </form>
    </div>
  );
}
/* ================= 图文正文（吃瓜） ================= */
export function ArticlePanel({ title, html, cover, hot, onClose }: {
  title: string; html: string; cover?: string; hot?: number; onClose: () => void;
}) {
  return (
    <div className="flex flex-col h-full">
      <div className="shrink-0 flex items-center gap-2 px-4 h-12">
        <span className="text-[13.5px] font-bold text-on">图文详情</span>
        {hot ? <span className="text-[10.5px] text-mut">{fmt(hot)} 热度</span> : null}
        <button onClick={onClose} aria-label="关闭" className="ml-auto w-7 h-7 rounded-full bg-s3 flex items-center justify-center text-mut pressable">
          <IClose size={13} />
        </button>
      </div>
      <div className="flex-1 scroll-y px-4 pb-6">
        {cover && <img src={cover} alt="" className="w-full rounded-(--r-md) mb-3 object-cover" />}
        <h2 className="text-[16px] font-bold text-on leading-snug">{title}</h2>
        <div className="article-body mt-3" dangerouslySetInnerHTML={{ __html: html }} />
        {!html && <p className="text-[12px] text-dim mt-4">这篇内容没有正文，可能只有图片或视频。</p>}
      </div>
    </div>
  );
}