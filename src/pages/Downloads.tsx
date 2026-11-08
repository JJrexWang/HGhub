import { useMemo, useState } from "react";
import { api, fmtBytes, platform } from "../api";
import type { DlTask } from "../types";
import { useStore } from "../store";
import { Empty, Img, Modal } from "../components/ui";
import { IBack, ICheck, IChevR, IDownload, IPauseF, IPlayF, IRefresh, ISearch, ITrash } from "../icons";

/* 下载管理（两级）：
   一级 = 作品列表（封面/已下集数/大小/更新徽标），可整部更新、整部删除、批量删作品；
   二级 = 单部作品的集数管理（播放/转 MP4/单集删除/暂停继续取消重试/批量删集/补下未下集）。
   线上更新：启动时 store 已逐部查过集数快照，有新集的作品封面带「更新 N 集」徽标；
   「一键更新库存」先刷新快照再把所有已下载作品的新集入队。 */

const ST_TEXT: Record<DlTask["status"], string> = {
  queued: "排队中", downloading: "下载中", paused: "已暂停",
  done: "已完成", failed: "失败", canceled: "已取消",
};
const HAVE_ST = new Set(["done", "queued", "downloading", "paused"]);

function fmtDate(ms: number) {
  if (!ms) return "";
  const d = new Date(ms);
  return `${d.getMonth() + 1}/${d.getDate()} ${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

type WorkAgg = {
  video_id: number; title: string; cover: string;
  tasks: DlTask[]; doneTasks: DlTask[]; activeTasks: DlTask[]; failedCount: number;
  size: number; total_episodes: number; latest: number; newEps: number[];
};

export default function Downloads({ onBack, wide }: { onBack: () => void; wide: boolean }) {
  const { downloads, toast, push, refreshDownloads, dlNewEps, dlSnap, dlChecking, bulk, updateDownloaded } = useStore();
  const [q, setQ] = useState("");
  const [sort, setSort] = useState<"recent" | "size" | "title">("recent");
  const [workId, setWorkId] = useState<number | null>(null);
  const [selectMode, setSelectMode] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [selectedWorks, setSelectedWorks] = useState<Set<number>>(new Set());
  const [confirm, setConfirm] = useState<{ text: string; run: () => void } | null>(null);
  const [converting, setConverting] = useState<string | null>(null);
  const [filling, setFilling] = useState(false);

  const act = (p: Promise<unknown>, okMsg?: string) => {
    p.then(() => { if (okMsg) toast(okMsg, "ok"); refreshDownloads(); })
      .catch((e) => toast((e as Error).message || "操作失败", "info"));
  };

  const works = useMemo<WorkAgg[]>(() => {
    const map = new Map<number, DlTask[]>();
    for (const t of downloads) {
      const arr = map.get(t.video_id) || [];
      arr.push(t);
      map.set(t.video_id, arr);
    }
    const kw = q.trim().toLowerCase();
    const list = [...map.entries()].map(([video_id, tasks]) => {
      const sorted = [...tasks].sort((a, b) => a.ep - b.ep);
      const doneTasks = sorted.filter((t) => t.status === "done");
      const snapLen = dlSnap[video_id]?.length || 0;
      return {
        video_id,
        title: (doneTasks[0] || sorted[0]).title || `作品 ${video_id}`,
        cover: (doneTasks[0] || sorted[0]).cover,
        tasks: sorted,
        doneTasks,
        activeTasks: sorted.filter((t) => t.status === "downloading" || t.status === "queued" || t.status === "paused"),
        failedCount: sorted.filter((t) => t.status === "failed").length,
        size: doneTasks.reduce((s, t) => s + t.size, 0),
        total_episodes: Math.max(0, ...tasks.map((t) => t.total_episodes || 0), snapLen),
        latest: Math.max(...tasks.map((t) => t.finished_at || t.created_at)),
        newEps: dlNewEps(video_id),
      };
    }).filter((w) => !kw || w.title.toLowerCase().includes(kw));
    if (sort === "size") list.sort((a, b) => b.size - a.size);
    else if (sort === "title") list.sort((a, b) => a.title.localeCompare(b.title, "zh"));
    else list.sort((a, b) => b.latest - a.latest);
    return list;
  }, [downloads, q, sort, dlSnap, dlNewEps]);

  const doneCount = downloads.filter((t) => t.status === "done").length;
  const totalBytes = downloads.filter((t) => t.status === "done").reduce((s, t) => s + t.size, 0);
  const activeCount = downloads.filter((t) => t.status === "downloading" || t.status === "queued").length;
  const updWorks = works.filter((w) => w.newEps.length > 0);
  const updEpsTotal = updWorks.reduce((s, w) => s + w.newEps.length, 0);

  const sel = workId != null ? works.find((w) => w.video_id === workId) || null : null;

  const playTask = (t: DlTask) => {
    push({ kind: "feed", videoIds: [t.video_id], index: 0, title: t.title || "已下载", startEp: t.ep });
  };

  const toggleSel = (id: string) => {
    setSelected((s) => {
      const n = new Set(s);
      if (n.has(id)) n.delete(id); else n.add(id);
      return n;
    });
  };
  const toggleWorkSel = (id: number) => {
    setSelectedWorks((s) => {
      const n = new Set(s);
      if (n.has(id)) n.delete(id); else n.add(id);
      return n;
    });
  };

  /** 二级页：把这部作品线上还没持有的集全部入队（失败/取消的集会重新入队续传） */
  const fillWork = async (w: WorkAgg) => {
    if (filling) return;
    setFilling(true);
    try {
      const d = await api.video(w.video_id);
      let eps = [...new Set((d.episodes || []).map((e) => e.ep_num).filter((n) => n > 0))].sort((a, b) => a - b);
      if (!eps.length && d.total_episodes > 0) eps = Array.from({ length: d.total_episodes }, (_, k) => k + 1);
      const have = new Set(w.tasks.filter((t) => HAVE_ST.has(t.status)).map((t) => t.ep));
      const todo = eps.filter((ep) => !have.has(ep));
      if (!todo.length) { toast("这部作品的集都已下载或在队列里", "info"); return; }
      await api.dlEnqueueSeries(w.video_id, todo);
      toast(`已入队 ${todo.length} 集`, "ok");
      refreshDownloads();
    } catch (e) {
      toast((e as Error).message || "读取线上集数失败", "info");
    } finally {
      setFilling(false);
    }
  };

  const exitSelect = () => { setSelectMode(false); setSelected(new Set()); setSelectedWorks(new Set()); };

  return (
    <div className="absolute inset-0 bg-bg flex flex-col">
      {sel ? (
        /* ── 二级：单部作品的集数管理 ─────────────────────────── */
        <>
          <div className="shrink-0 px-3 pt-2 pb-3 flex items-center gap-2" style={{ paddingTop: "calc(var(--sat, 0px) + 10px)" }}>
            <button onClick={() => { setWorkId(null); exitSelect(); }} aria-label="返回作品列表" className="w-10 h-10 rounded-full flex items-center justify-center text-on hover:bg-s3 pressable"><IBack size={20} /></button>
            <div className="relative w-9 h-[46px] rounded-(--r-sm) overflow-hidden bg-s3 shrink-0">
              {sel.cover && <Img src={sel.cover} />}
            </div>
            <div className="flex-1 min-w-0">
              <div className="text-[16px] font-bold text-on leading-tight truncate">{sel.title}</div>
              <div className="text-[11px] text-mut mt-0.5">
                已下 {sel.doneTasks.length}{sel.total_episodes ? ` / ${sel.total_episodes}` : ""} 集 · {fmtBytes(sel.size)}
                {sel.newEps.length > 0 && <span className="text-[#e5534b] font-semibold"> · 线上新出 {sel.newEps.length} 集</span>}
              </div>
            </div>
            <button onClick={() => { setSelectMode((v) => !v); setSelected(new Set()); }}
              className={`h-9 px-3 rounded-(--r-ctl) text-[12px] pressable ${selectMode ? "bg-primary text-on-primary font-semibold" : "bg-s3 text-on"}`}>
              {selectMode ? "完成" : "批量管理"}
            </button>
          </div>

          <div className="shrink-0 px-4 pb-2 flex items-center gap-2">
            {sel.doneTasks.length > 0 && (
              <button onClick={() => playTask(sel.doneTasks[0])} className="flex items-center gap-1.5 h-9 px-3 rounded-(--r-ctl) bg-s3 text-[12px] text-on pressable">
                <IPlayF size={13} />播放
              </button>
            )}
            {sel.newEps.length > 0 && (
              <button onClick={() => act(api.dlEnqueueSeries(sel.video_id, sel.newEps), `已入队 ${sel.newEps.length} 集新集`)}
                className="flex items-center gap-1.5 h-9 px-3 rounded-(--r-ctl) bg-[#e5534b] text-white text-[12px] font-semibold pressable">
                <IDownload size={13} />下载新集（{sel.newEps.length}）
              </button>
            )}
            <button disabled={filling} onClick={() => { void fillWork(sel); }}
              className="flex items-center gap-1.5 h-9 px-3 rounded-(--r-ctl) bg-s3 text-[12px] text-on pressable disabled:opacity-50">
              <IDownload size={13} />{filling ? "读取中…" : "补下未下集"}
            </button>
            <button onClick={() => setConfirm({
              text: `删除《${sel.title}》的全部 ${sel.tasks.length} 集下载文件与任务？`,
              run: () => { setWorkId(null); exitSelect(); act(api.dlRemoveWork(sel.video_id), "已删除整部"); },
            })} className="ml-auto h-9 px-3 rounded-(--r-ctl) bg-s3 text-[12px] text-mut pressable">删整部</button>
          </div>

          <div className="flex-1 scroll-y px-4 pb-10">
            <div className={wide ? "max-w-[820px] mx-auto" : ""}>
              <div className="rounded-(--r-md) bg-s2 divide-y divide-s3 overflow-hidden">
                {sel.tasks.map((t) => {
                  const pct = t.segs_total ? Math.min(100, Math.floor((t.segs_done / t.segs_total) * 100)) : 0;
                  return (
                    <div key={t.id} className="px-3.5 py-2.5">
                      <div className="flex items-center gap-2">
                        {selectMode && t.status === "done" && (
                          <button onClick={() => toggleSel(t.id)} aria-label="选择"
                            className={`w-5 h-5 rounded-md flex items-center justify-center pressable shrink-0 ${selected.has(t.id) ? "bg-primary text-on-primary" : "bg-s4 text-transparent"}`}>
                            <ICheck size={12} />
                          </button>
                        )}
                        <div className="flex-1 min-w-0">
                          <div className="text-[12.5px] text-on truncate">
                            第 {t.ep} 集{t.ep_title ? <span className="text-mut"> · {t.ep_title}</span> : null}
                            {t.status === "done" && t.file_name ? <span className="text-dim"> · {t.file_name}</span> : null}
                          </div>
                          <div className="text-[10.5px] text-mut mt-0.5">
                            {ST_TEXT[t.status]}
                            {t.status === "done" && <> · {fmtBytes(t.size)} · {fmtDate(t.finished_at)}</>}
                            {t.status === "downloading" && t.segs_total > 0 && <> · {t.segs_done}/{t.segs_total} 片 · {fmtBytes(t.bytes_done)}</>}
                            {t.status === "failed" && t.error && <span className="text-[#e5534b]"> · {t.error}</span>}
                          </div>
                        </div>
                        {t.status === "done" && (
                          <button onClick={() => playTask(t)} className="flex items-center gap-1 h-8 px-2.5 rounded-(--r-ctl) bg-s3 text-[11px] text-on pressable">
                            <IPlayF size={12} />播放
                          </button>
                        )}
                        {t.status === "done" && t.format === "ts" && !selectMode && (
                          <button disabled={converting === t.id}
                            onClick={() => {
                              setConverting(t.id);
                              const p = api.dlRemux(t.id);
                              act(p, "已无损转为 MP4");
                              p.then(() => setConverting(null), () => setConverting(null));
                            }}
                            className="flex items-center gap-1 h-8 px-2.5 rounded-(--r-ctl) bg-s3 text-[11px] text-on pressable disabled:opacity-50">
                            {converting === t.id ? "转换中…" : "转 MP4"}
                          </button>
                        )}
                        {t.status === "downloading" && (
                          <button onClick={() => act(api.dlPause(t.id))} aria-label="暂停" className="w-8 h-8 rounded-full bg-s3 flex items-center justify-center text-on pressable"><IPauseF size={14} /></button>
                        )}
                        {(t.status === "paused" || t.status === "failed" || t.status === "canceled") && (
                          <button onClick={() => act(api.dlResume(t.id), "已继续下载")} aria-label="继续" className="w-8 h-8 rounded-full bg-s3 flex items-center justify-center text-on pressable">
                            {t.status === "failed" ? <IRefresh size={14} /> : <IPlayF size={14} />}
                          </button>
                        )}
                        {t.status === "queued" && (
                          <button onClick={() => act(api.dlCancel(t.id))} aria-label="取消排队" className="h-8 px-2.5 rounded-full bg-s3 flex items-center justify-center text-mut pressable"><span className="text-[11px]">取消</span></button>
                        )}
                        {!selectMode && (
                          <button onClick={() => setConfirm({
                            text: t.status === "done" ? `删除文件「${t.file_name || `第 ${t.ep} 集`}」？` : `删除「${sel.title} 第 ${t.ep} 集」的下载任务和已下载的分片？`,
                            run: () => act(api.dlRemove(t.id), "已删除"),
                          })}
                            aria-label="删除" className="w-8 h-8 rounded-full bg-s3 flex items-center justify-center text-mut pressable"><ITrash size={14} /></button>
                        )}
                      </div>
                      {(t.status === "downloading" || t.status === "paused") && t.segs_total > 0 && (
                        <div className="mt-2 h-[4px] rounded-full bg-s4 overflow-hidden">
                          <div className="h-full bg-primary rounded-full" style={{ width: `${pct}%`, transition: "width .3s" }} />
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            </div>
          </div>

          {selectMode && selected.size > 0 && (
            <div className="shrink-0 px-4 py-3 bg-s2 flex items-center gap-3" style={{ paddingBottom: "calc(var(--sab, 0px) + 12px)" }}>
              <span className="text-[12px] text-on">已选 {selected.size} 集</span>
              <button onClick={() => setConfirm({
                text: `删除选中的 ${selected.size} 集下载文件？`,
                run: () => {
                  const ids = [...selected];
                  setSelected(new Set());
                  Promise.allSettled(ids.map((id) => api.dlRemove(id))).then(() => { toast("已删除所选", "ok"); refreshDownloads(); });
                },
              })} className="ml-auto h-9 px-4 rounded-(--r-ctl) bg-[#e5534b] text-white text-[12.5px] font-semibold pressable">删除所选</button>
            </div>
          )}
        </>
      ) : (
        /* ── 一级：作品列表 ─────────────────────────────────── */
        <>
          <div className="shrink-0 px-3 pt-2 pb-3 flex items-center gap-2" style={{ paddingTop: "calc(var(--sat, 0px) + 10px)" }}>
            <button onClick={onBack} aria-label="返回" className="w-10 h-10 rounded-full flex items-center justify-center text-on hover:bg-s3 pressable"><IBack size={20} /></button>
            <div className="flex-1 min-w-0">
              <div className="text-[20px] font-bold text-on leading-none">下载管理</div>
              <div className="text-[11px] text-mut mt-1">
                {works.length} 部作品 · 已下载 {doneCount} 集 · 共 {fmtBytes(totalBytes)}
                {activeCount > 0 && <> · 进行中 {activeCount}</>}
                {updWorks.length > 0 && <span className="text-[#e5534b] font-semibold"> · {updWorks.length} 部有更新</span>}
                {dlChecking && <> · 正在检查更新…</>}
              </div>
            </div>
            {platform !== "android" && (
              <button onClick={() => act(api.dlOpenDir())} className="h-9 px-3 rounded-(--r-ctl) bg-s3 text-[12px] text-on pressable">打开目录</button>
            )}
            <button onClick={() => { setSelectMode((v) => !v); setSelectedWorks(new Set()); }}
              className={`h-9 px-3 rounded-(--r-ctl) text-[12px] pressable ${selectMode ? "bg-primary text-on-primary font-semibold" : "bg-s3 text-on"}`}>
              {selectMode ? "完成" : "批量管理"}
            </button>
          </div>

          <div className="shrink-0 px-4 pb-2 flex items-center gap-2">
            <button disabled={!!bulk || dlChecking} onClick={() => { void updateDownloaded(); }}
              className="flex items-center gap-1.5 h-9 px-3 rounded-(--r-ctl) bg-primary text-on-primary text-[12px] font-semibold pressable disabled:opacity-60 shrink-0">
              <IRefresh size={13} />
              {bulk?.kind === "updates" ? `更新中 ${bulk.done}/${bulk.total}` : dlChecking ? "检查中…" : "一键更新库存"}
              {!bulk && updEpsTotal > 0 && <span className="px-1.5 py-0.5 rounded-full bg-white/25 text-[10px] leading-none">{updEpsTotal} 集</span>}
            </button>
            <div className="flex-1 flex items-center gap-2 h-9 px-3 rounded-(--r-ctl) bg-s3">
              <ISearch size={15} className="text-mut shrink-0" />
              <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="搜索作品名"
                className="flex-1 min-w-0 bg-transparent outline-none text-[12.5px] text-on placeholder-dim" />
            </div>
            {([["recent", "最近"], ["size", "大小"], ["title", "名称"]] as const).map(([k, l]) => (
              <button key={k} onClick={() => setSort(k)}
                className={`h-9 px-2.5 rounded-(--r-ctl) text-[11.5px] pressable ${sort === k ? "tonal text-on-sec font-semibold" : "text-mut"}`}
                style={sort === k ? { background: "var(--p-sec)", color: "var(--p-primary)" } : undefined}>{l}</button>
            ))}
          </div>

          <div className="flex-1 scroll-y px-4 pb-10">
            <div className={wide ? "max-w-[820px] mx-auto" : ""}>
              {works.length > 0 ? (
                <div className="flex flex-col gap-2.5 mt-1">
                  {works.map((w) => {
                    const dlTask = w.tasks.find((t) => t.status === "downloading");
                    const pct = dlTask?.segs_total ? Math.min(100, Math.floor((dlTask.segs_done / dlTask.segs_total) * 100)) : 0;
                    return (
                      <div key={w.video_id} onClick={() => { if (selectMode) toggleWorkSel(w.video_id); else { setWorkId(w.video_id); exitSelect(); } }}
                        className="rounded-(--r-md) bg-s2 overflow-hidden flex items-center gap-3 px-3.5 py-3 pressable cursor-pointer">
                        {selectMode && (
                          <span className={`w-5 h-5 rounded-md flex items-center justify-center shrink-0 ${selectedWorks.has(w.video_id) ? "bg-primary text-on-primary" : "bg-s4 text-transparent"}`}>
                            <ICheck size={12} />
                          </span>
                        )}
                        <div className="relative w-11 h-[58px] rounded-(--r-sm) overflow-hidden bg-s3 shrink-0">
                          {w.cover && <Img src={w.cover} />}
                          {w.newEps.length > 0 && (
                            <span className="absolute top-1 left-1 px-1.5 py-0.5 rounded-full bg-[#e5534b] text-[9px] font-semibold text-white leading-none">
                              更新 {w.newEps.length} 集
                            </span>
                          )}
                          {dlTask && (
                            <span className="absolute inset-x-0 bottom-0 h-[3px] bg-black/40">
                              <span className="block h-full bg-primary" style={{ width: `${pct}%` }} />
                            </span>
                          )}
                        </div>
                        <div className="flex-1 min-w-0">
                          <div className="text-[13px] font-semibold text-on truncate">{w.title}</div>
                          <div className="text-[10.5px] text-mut mt-0.5">
                            已下 {w.doneTasks.length}{w.total_episodes ? ` / ${w.total_episodes}` : ""} 集 · {fmtBytes(w.size)} · 最近 {fmtDate(w.latest)}
                          </div>
                          {(w.activeTasks.length > 0 || w.failedCount > 0) && (
                            <div className="text-[10.5px] mt-0.5">
                              {w.activeTasks.length > 0 && <span className="text-primary font-medium">{w.activeTasks.some((t) => t.status === "downloading") ? "下载中" : "队列中"} {w.activeTasks.length} 集</span>}
                              {w.failedCount > 0 && <span className="text-[#e5534b]"> · {w.failedCount} 集失败</span>}
                            </div>
                          )}
                        </div>
                        {w.newEps.length > 0 && !selectMode && (
                          <button onClick={(e) => { e.stopPropagation(); act(api.dlEnqueueSeries(w.video_id, w.newEps), `已入队 ${w.newEps.length} 集新集`); }}
                            className="flex items-center gap-1 h-8 px-2.5 rounded-(--r-ctl) bg-[#e5534b] text-white text-[11px] font-semibold pressable shrink-0">
                            <IDownload size={12} />更新
                          </button>
                        )}
                        {!selectMode && (
                          <button onClick={(e) => {
                            e.stopPropagation();
                            setConfirm({ text: `删除《${w.title}》的全部 ${w.tasks.length} 集下载文件与任务？`, run: () => act(api.dlRemoveWork(w.video_id), "已删除整部") });
                          }}
                            aria-label="删整部" className="w-8 h-8 rounded-full bg-s3 flex items-center justify-center text-mut pressable shrink-0"><ITrash size={14} /></button>
                        )}
                        <IChevR size={16} className="text-dim shrink-0" />
                      </div>
                    );
                  })}
                </div>
              ) : downloads.length ? (
                <Empty icon={<ISearch size={24} />} title="没有匹配的作品" sub="换个关键词试试" />
              ) : (
                <Empty icon={<IDownload size={24} />} title="还没有下载"
                  sub="在播放页点「下载」或在选集里选「整部下载」，下载好的剧会出现在这里，断网也能看"
                  action="去逛逛" onAction={onBack} />
              )}
            </div>
          </div>

          {selectMode && selectedWorks.size > 0 && (
            <div className="shrink-0 px-4 py-3 bg-s2 flex items-center gap-3" style={{ paddingBottom: "calc(var(--sab, 0px) + 12px)" }}>
              <span className="text-[12px] text-on">已选 {selectedWorks.size} 部</span>
              <button onClick={() => setConfirm({
                text: `删除选中的 ${selectedWorks.size} 部作品的全部下载文件与任务？`,
                run: () => {
                  const ids = [...selectedWorks];
                  setSelectedWorks(new Set());
                  Promise.allSettled(ids.map((id) => api.dlRemoveWork(id))).then(() => { toast("已删除所选作品", "ok"); refreshDownloads(); });
                },
              })} className="ml-auto h-9 px-4 rounded-(--r-ctl) bg-[#e5534b] text-white text-[12.5px] font-semibold pressable">删除所选</button>
            </div>
          )}
        </>
      )}

      <Modal open={!!confirm} onClose={() => setConfirm(null)} width={330}>
        <div className="p-5">
          <div className="text-[14.5px] font-bold text-on">确认删除</div>
          <p className="text-[12px] text-mut leading-relaxed mt-2">{confirm?.text}</p>
          <div className="flex gap-2 mt-5">
            <button onClick={() => setConfirm(null)} className="flex-1 h-9 rounded-(--r-ctl) bg-s3 text-[12.5px] text-on/80 pressable">取消</button>
            <button onClick={() => { const c = confirm; setConfirm(null); c?.run(); }}
              className="flex-1 h-9 rounded-(--r-ctl) bg-[#e5534b] text-[12.5px] font-semibold text-white pressable">删除</button>
          </div>
        </div>
      </Modal>
    </div>
  );
}
