import { useEffect, useState } from "react";
import { api, APP_VERSION, fmt, fmtBytes, fmtTime, platform } from "../api";
import type { FavCard } from "../types";
import { useStore } from "../store";
import { Avatar, Empty, Img } from "../components/ui";
import { IChevR, IClock, ILogout, ISettings, IStar, IUser } from "../icons";

/* 我的：结构固定（资料卡 + 我的收藏 / 观看历史）。
   收藏：已登录时以网站收藏为准（进应用/登录后自动同步），未登录用本地收藏；
   观看记录仍是本地的。未登录时资料卡本身就是登录入口。 */
export default function Profile({ wide }: { wide: boolean }) {
  const { user, setLoginOpen, logout, history, favs, favCards, favsSynced, syncFavs, clearHistory, push, downloads } = useStore();
  const [tab, setTab] = useState<"fav" | "history" | "dl">("fav");
  const [cards, setCards] = useState<FavCard[]>([]);
  const [loading, setLoading] = useState(false);

  const ids = tab === "fav" ? favs : tab === "history" ? history.map((h) => h.id) : [];
  /** 下载按作品聚合（已完成集） */
  const dlWorks = (() => {
    const map = new Map<number, { title: string; cover: string; eps: number[]; size: number; firstEp: number }>();
    for (const t of downloads) {
      if (t.status !== "done") continue;
      const w = map.get(t.video_id) || { title: t.title, cover: t.cover, eps: [], size: 0, firstEp: t.ep };
      w.eps.push(t.ep);
      w.size += t.size;
      w.firstEp = Math.min(w.firstEp, t.ep);
      map.set(t.video_id, w);
    }
    return [...map.entries()].map(([video_id, w]) => ({ video_id, ...w }));
  })();
  const dlActive = downloads.filter((t) => t.status === "downloading" || t.status === "queued").length;
  /** 观看记录里这条的进度（收藏页没有） */
  const progressOf = (id: number) => (tab === "history" ? history.find((h) => h.id === id) : undefined);

  useEffect(() => {
    let alive = true;
    /* 收藏页：网站同步下来的卡片快照直接用；缺快照的（刚收藏的）再按 id 补拉详情。
       历史页维持原样：按 id 拉详情，最多 12 条。 */
    const want = tab === "fav" ? ids : ids.slice(0, 12);
    if (!want.length) { setCards([]); return; }
    const byId = new Map<number, FavCard>();
    if (tab === "fav") for (const c of favCards) byId.set(c.id, c);
    const missing = want.filter((id) => !byId.has(id)).slice(0, tab === "fav" ? 30 : 12);
    const finish = () => {
      if (alive) setCards(want.map((id) => byId.get(id)).filter((c): c is FavCard => !!c));
    };
    if (!missing.length) { finish(); return; }
    setLoading(true);
    Promise.allSettled(missing.map((id) => api.video(id)))
      .then((rs) => {
        for (const r of rs) {
          if (r.status === "fulfilled") {
            const d = r.value;
            byId.set(d.id, { id: d.id, title: d.title, cover: d.cover });
          }
        }
        finish();
      })
      .finally(() => { if (alive) setLoading(false); });
    return () => { alive = false; };
  }, [tab, ids.join(","), favCards]);

  return (
    <div className="absolute inset-0 bg-bg flex flex-col">
      <div className="shrink-0 px-4 pb-2 flex items-center" style={{ paddingTop: "calc(var(--sat, 0px) + 14px)" }}>
        <div className="text-[24px] font-bold text-on leading-none">我的</div>
        {platform === "android" && (
          <button onClick={() => push({ kind: "settings" })} aria-label="设置"
            className="ml-auto w-9 h-9 rounded-full bg-s3 flex items-center justify-center text-mut pressable">
            <ISettings size={17} strokeWidth={1.7} />
          </button>
        )}
      </div>

      <div className="flex-1 scroll-y px-4 pt-2 pb-24">
        {/* 资料卡：已登录 / 未登录（点开登录抽屉） */}
        {user ? (
          <div className="rounded-(--r-lg) bg-s1 p-4 elev-1 flex items-center gap-3.5 a-rise">
            <Avatar size={56} name={user.nickname} hue={280} ring />
            <div className="flex-1 min-w-0">
              <div className="text-[16px] font-bold text-on truncate">{user.nickname}</div>
              <div className="text-[11px] text-mut mt-0.5">@{user.username} · ID {user.id}</div>
              <div className="flex gap-3 mt-2 text-[10.5px] text-mut">
                <span>点赞 <b className="text-on">{fmt(user.counts?.like)}</b></span>
                <span>收藏 <b className="text-on">{favsSynced ? favs.length : fmt(user.counts?.collect)}</b></span>
                <span>关注 <b className="text-on">{fmt(user.counts?.following)}</b></span>
              </div>
            </div>
            <button onClick={async () => { await logout(); }}
              className="w-9 h-9 rounded-full bg-s3 flex items-center justify-center text-mut pressable" aria-label="退出登录">
              <ILogout size={17} />
            </button>
          </div>
        ) : (
          <button onClick={() => setLoginOpen(true)}
            className="w-full text-left rounded-(--r-lg) bg-s1 p-4 elev-1 flex items-center gap-3.5 pressable a-rise">
            <Avatar size={56} icon={<IUser size={26} />} hue={220} ring />
            <div className="flex-1 min-w-0">
              <div className="text-[16px] font-bold text-on">未登录</div>
              <div className="text-[11px] text-mut mt-0.5">登录后可同步点赞、收藏与评论</div>
              <div className="flex gap-3 mt-2 text-[10.5px] text-mut">
                <span>收藏 <b className="text-on">{favs.length}</b></span>
                <span>观看 <b className="text-on">{history.length}</b></span>
              </div>
            </div>
            <span className="flex items-center gap-0.5 h-8 pl-3 pr-2 rounded-full bg-primary text-on-primary text-[11.5px] font-semibold">
              登录<IChevR size={13} />
            </span>
          </button>
        )}

        {/* 我的内容 */}
        <div className="flex bg-s3 rounded-(--r-ctl) p-1 mt-4">
          {([["fav", "我的收藏"], ["history", "观看历史"], ["dl", "下载"]] as const).map(([k, l]) => (
            <button key={k} onClick={() => setTab(k)}
              className={`flex-1 h-8 rounded-(--r-ctl) text-[12px] font-semibold pressable ${tab === k ? "tonal text-on-sec" : "text-mut"}`}
              style={tab === k ? { background: "var(--p-sec)", color: "var(--p-primary)" } : undefined}>{l}</button>
          ))}
        </div>

        <div className="mt-3">
          {tab === "dl" ? (
            dlWorks.length || dlActive ? (
              <div>
                {dlActive > 0 && (
                  <button onClick={() => push({ kind: "downloads" })}
                    className="w-full mb-2 h-9 rounded-(--r-ctl) bg-s3 text-[12px] text-on pressable">
                    {dlActive} 个任务下载中，点此查看进度
                  </button>
                )}
                <div className="grid grid-cols-3 gap-2">
                  {dlWorks.map((w) => (
                    <button key={w.video_id} onClick={() => push({
                      kind: "feed", videoIds: [w.video_id], index: 0, title: w.title, startEp: w.firstEp,
                    })}
                      className="relative rounded-(--r-sm) overflow-hidden bg-s1 elev-1 pressable" style={{ paddingTop: "133%" }}>
                      <Img src={w.cover} />
                      <div className="absolute top-1.5 left-1.5 px-1.5 py-0.5 rounded-full bg-black/65 text-[9.5px] text-white/90 leading-none">
                        已下 {w.eps.length} 集
                      </div>
                      <div className="absolute inset-x-0 bottom-0 bg-gradient-to-t from-black/85 to-transparent p-1.5">
                        <div className="text-[10.5px] text-white clamp-2 leading-tight">{w.title}</div>
                        <div className="text-[9px] text-white/60">{fmtBytes(w.size)}</div>
                      </div>
                    </button>
                  ))}
                </div>
                <button onClick={() => push({ kind: "downloads" })} className="mt-3 w-full h-9 rounded-(--r-ctl) bg-s3 text-[12px] text-mut pressable">下载管理（删除 / 批量 / 存储）</button>
              </div>
            ) : (
              <Empty icon={<IClock size={24} />} title="还没有下载"
                sub="在播放页点「下载」即可离线保存，下载好的剧会出现在这里"
                action="去逛逛" onAction={() => push({ kind: "ranks" })} />
            )
          ) : loading ? (
            <div className="flex justify-center py-8"><div className="w-7 h-7 spinner" /></div>
          ) : cards.length ? (
            <div className="grid grid-cols-3 gap-2">
              {cards.map((c) => {
                const p = progressOf(c.id);
                return (
                <button key={c.id} onClick={() => push({
                  kind: "feed", videoIds: cards.map((x) => x.id), index: cards.findIndex((x) => x.id === c.id),
                  title: tab === "fav" ? "我的收藏" : "观看历史",
                  startEp: p?.ep, startTime: p?.t,
                })}
                  className="relative rounded-(--r-sm) overflow-hidden bg-s1 elev-1 pressable" style={{ paddingTop: "133%" }}>
                  <Img src={c.cover} />
                  {!!p && (
                    <div className="absolute top-1.5 left-1.5 px-1.5 py-0.5 rounded-full bg-black/65 text-[9.5px] text-white/90 leading-none">
                      第 {p.ep} 集{p.t > 3 ? ` · ${fmtTime(p.t)}` : ""}
                    </div>
                  )}
                  <div className="absolute inset-x-0 bottom-0 bg-gradient-to-t from-black/85 to-transparent p-1.5">
                    <div className="text-[10.5px] text-white clamp-2 leading-tight">{c.title}</div>
                  </div>
                </button>
                );
              })}
            </div>
          ) : (
            tab === "fav" && user && !favsSynced ? (
              <Empty icon={<IStar size={24} />}
                title="网站收藏还没同步上"
                sub="网络不通时这里先按本地收藏显示，恢复后点重试同步网站收藏"
                action="重试同步" onAction={() => { void syncFavs(); }} />
            ) : (
              <Empty icon={tab === "fav" ? <IStar size={24} /> : <IClock size={24} />}
                title={tab === "fav" ? "还没有收藏" : "还没有观看记录"}
                sub={tab === "fav" ? "在剧集页点收藏，之后就能在这里找到" : "看过的剧集会出现在这里"}
                action="去逛逛" onAction={() => push({ kind: "ranks" })} />
            )
          )}
          {cards.length > 0 && tab === "history" && (
            <button onClick={clearHistory} className="mt-3 w-full h-9 rounded-(--r-ctl) bg-s3 text-[12px] text-mut pressable">清空观看历史</button>
          )}
        </div>

        <div className="text-center text-[10px] text-dim mt-5">HGhub {APP_VERSION} · {platform === "android" ? "Android" : platform === "macos" ? "macOS" : "Windows"} 客户端</div>
      </div>
    </div>
  );
}