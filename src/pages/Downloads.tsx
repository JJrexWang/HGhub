import { useMemo, useState } from "react";
import { api, fmtBytes, platform } from "../api";
import type { DlTask } from "../types";
import { useStore } from "../store";
import { Empty, Img, Modal } from "../components/ui";
import { IBack, ICheck, IDownload, IPauseF, IPlayF, IRefresh, ISearch, ITrash } from "../icons";

/* 下载管理：进行中队列（暂停/继续/取消/重试/删除）+ 已完成按作品聚合
   （播放/单集删除/整部删除/批量多选删除）+ 存储统计与打开目录。
   数据来自 Rust 侧 DownloadManager（store.downloads 镜像），离线打开照常可用。 */

const ST_TEXT: Record<DlTask["status"], string> = {
  queued: "排队中", downloading: "下载中", paused: "已暂停",
  done: "已完成", failed: "失败", canceled: "已取消",
};

function fmtDate(ms: number) {
  if (!ms) return "";
  const d = new Date(ms);
  return `${d.getMonth() + 1}/${d.getDate()} ${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

export default function Downloads({ onBack, wide }: { onBack: () => void; wide: boolean }) {
  const { downloads, toast, push, refreshDownloads } = useStore();
  const [q, setQ] = useState("");
  const [sort, setSort] = useState<"recent" | "size" | "title">("recent");
  const [selectMode, setSelectMode] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [confirm, setConfirm] = useState<{ text: string; run: () => void } | null>(null);
  const [converting, setConverting] = useState<string | null>(null);

  const act = (p: Promise<unknown>, okMsg?: string) => {
    p.then(() => { if (okMsg) toast(okMsg, "ok"); refreshDownloads(); })
      .catch((e) => toast((e as Error).message || "操作失败", "info"));
  };

  const kw = q.trim().toLowerCase();
  const match = (t: DlTask) => !kw || t.title.toLowerCase().includes(kw) || (t.file_name || "").toLowerCase().includes(kw);

  const active = downloads.filter((t) => t.status !== "done" && match(t));
  const done = downloads.filter((t) => t.status === "done" && match(t));

  const works = useMemo(() => {
    const map = new Map<number, DlTask[]>();
    for (const t of done) {
      const arr = map.get(t.video_id) || [];
      arr.push(t);
      map.set(t.video_id, arr);
    }
    const list = [...map.entries()].map(([video_id, tasks]) => ({
      video_id,
      title: tasks[0].title || `作品 ${video_id}`,
      cover: tasks[0].cover,
      total_episodes: Math.max(...tasks.map((t) => t.total_episodes || 0)),
      tasks: [...tasks].sort((a, b) => a.ep - b.ep),
      size: tasks.reduce((s, t) => s + t.size, 0),
      latest: Math.max(...tasks.map((t) => t.finished_at || t.created_at)),
    }));
    if (sort === "size") list.sort((a, b) => b.size - a.size);
    else if (sort === "title") list.sort((a, b) => a.title.localeCompare(b.title, "zh"));
    else list.sort((a, b) => b.latest - a.latest);
    return list;
  }, [done, sort]);

  const totalBytes = downloads.filter((t) => t.status === "done").reduce((s, t) => s + t.size, 0);
  const doneCount = downloads.filter((t) => t.status === "done").length;

  const toggleSel = (id: string) => {
    setSelected((s) => {
      const n = new Set(s);
      if (n.has(id)) n.delete(id); else n.add(id);
      return n;
    });
  };

  const playTask = (t: DlTask) => {
    push({
      kind: "feed", videoIds: [t.video_id], index: 0,
      title: t.title || "已下载", startEp: t.ep,
    });
  };

  return (
    <div className="absolute inset-0 bg-bg flex flex-col">
      <div className="shrink-0 px-3 pt-2 pb-3 flex items-center gap-2" style={{ paddingTop: "calc(var(--sat, 0px) + 10px)" }}>
        <button onClick={onBack} aria-label="返回" className="w-10 h-10 rounded-full flex items-center justify-center text-on hover:bg-s3 pressable"><IBack size={20} /></button>
        <div className="flex-1 min-w-0">
          <div className="text-[20px] font-bold text-on leading-none">下载管理</div>
          <div className="text-[11px] text-mut mt-1">
            已下载 {doneCount} 集 · 共 {fmtBytes(totalBytes)}
            {active.length > 0 && <> · 进行中 {active.length}</>}
          </div>
        </div>
        {platform !== "android" && (
          <button onClick={() => act(api.dlOpenDir())} className="h-9 px-3 rounded-(--r-ctl) bg-s3 text-[12px] text-on pressable">打开目录</button>
        )}
        <button onClick={() => { setSelectMode((v) => !v); setSelected(new Set()); }}
          className={`h-9 px-3 rounded-(--r-ctl) text-[12px] pressable ${selectMode ? "bg-primary text-on-primary font-semibold" : "bg-s3 text-on"}`}>
          {selectMode ? "完成" : "批量管理"}
        </button>
      </div>

      <div className="shrink-0 px-4 pb-2 flex items-center gap-2">
        <div className="flex-1 flex items-center gap-2 h-9 px-3 rounded-(--r-ctl) bg-s3">
          <ISearch size={15} className="text-mut shrink-0" />
          <input value={q} onChange={(e) => setQ(e.target.value)} placeholder="搜索作品名 / 文件名"
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
          {/* 进行中 */}
          {active.length > 0 && (
            <>
              <div className="text-[10.5px] font-semibold text-dim tracking-wide px-1 mt-2 mb-1.5">进行中（全局顺序下载，一个任务跑完再下一个）</div>
              <div className="rounded-(--r-md) bg-s2 divide-y divide-s3 overflow-hidden">
                {active.map((t) => {
                  const pct = t.segs_total ? Math.min(100, Math.floor((t.segs_done / t.segs_total) * 100)) : 0;
                  return (
                    <div key={t.id} className="px-3.5 py-3">
                      <div className="flex items-center gap-2">
                        <div className="flex-1 min-w-0">
                          <div className="text-[12.5px] text-on truncate">{t.title || `作品 ${t.video_id}`} · 第 {t.ep} 集</div>
                          <div className="text-[10.5px] text-mut mt-0.5">
                            {ST_TEXT[t.status]}
                            {t.status === "downloading" && t.segs_total > 0 && <> · {t.segs_done}/{t.segs_total} 片 · {fmtBytes(t.bytes_done)}</>}
                            {t.status === "failed" && t.error && <span className="text-[#e5534b]"> · {t.error}</span>}
                          </div>
                        </div>
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
                        <button onClick={() => setConfirm({ text: `删除「${t.title} 第 ${t.ep} 集」的下载任务和已下载的分片？`, run: () => act(api.dlRemove(t.id), "已删除") })}
                          aria-label="删除" className="w-8 h-8 rounded-full bg-s3 flex items-center justify-center text-mut pressable"><ITrash size={15} /></button>
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
            </>
          )}

          {/* 已完成：按作品聚合 */}
          {works.length > 0 && (
            <>
              <div className="text-[10.5px] font-semibold text-dim tracking-wide px-1 mt-4 mb-1.5">已下载（按作品）</div>
              <div className="flex flex-col gap-3">
                {works.map((w) => (
                  <div key={w.video_id} className="rounded-(--r-md) bg-s2 overflow-hidden">
                    <div className="flex items-center gap-3 px-3.5 py-3">
                      <div className="relative w-10 h-[52px] rounded-(--r-sm) overflow-hidden bg-s3 shrink-0">
                        {w.cover && <Img src={w.cover} />}
                      </div>
                      <div className="flex-1 min-w-0">
                        <div className="text-[13px] font-semibold text-on truncate">{w.title}</div>
                        <div className="text-[10.5px] text-mut mt-0.5">
                          已下 {w.tasks.length}{w.total_episodes ? ` / ${w.total_episodes}` : ""} 集 · {fmtBytes(w.size)} · 最近 {fmtDate(w.latest)}
                        </div>
                      </div>
                      <button onClick={() => setConfirm({ text: `删除《${w.title}》的全部 ${w.tasks.length} 集下载文件？`, run: () => act(api.dlRemoveWork(w.video_id), "已删除整部") })}
                        className="h-8 px-2.5 rounded-(--r-ctl) bg-s3 text-[11px] text-mut pressable">删整部</button>
                    </div>
                    <div className="divide-y divide-s3">
                      {w.tasks.map((t) => (
                        <div key={t.id} className="flex items-center gap-2 px-3.5 py-2">
                          {selectMode && (
                            <button onClick={() => toggleSel(t.id)} aria-label="选择"
                              className={`w-5 h-5 rounded-md flex items-center justify-center pressable ${selected.has(t.id) ? "bg-primary text-on-primary" : "bg-s4 text-transparent"}`}>
                              <ICheck size={12} />
                            </button>
                          )}
                          <div className="flex-1 min-w-0">
                            <div className="text-[12px] text-on truncate">{t.file_name || `第 ${t.ep} 集`}</div>
                            <div className="text-[10px] text-dim">{fmtBytes(t.size)} · {fmtDate(t.finished_at)}</div>
                          </div>
                          <button onClick={() => playTask(t)} className="flex items-center gap-1 h-8 px-2.5 rounded-(--r-ctl) bg-s3 text-[11px] text-on pressable">
                            <IPlayF size={12} />播放
                          </button>
                          {t.format === "ts" && !selectMode && (
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
                          {!selectMode && (
                            <button onClick={() => setConfirm({ text: `删除文件「${t.file_name || `第 ${t.ep} 集`}」？`, run: () => act(api.dlRemove(t.id), "已删除") })}
                              aria-label="删除本集" className="w-8 h-8 rounded-full bg-s3 flex items-center justify-center text-mut pressable"><ITrash size={14} /></button>
                          )}
                        </div>
                      ))}
                    </div>
                  </div>
                ))}
              </div>
            </>
          )}

          {!downloads.length && (
            <Empty icon={<IDownload size={24} />} title="还没有下载"
              sub="在播放页点「下载」或在选集里选「整部下载」，下载好的剧会出现在这里，断网也能看"
              action="去逛逛" onAction={onBack} />
          )}
          {!!downloads.length && !active.length && !works.length && (
            <Empty icon={<ISearch size={24} />} title="没有匹配的下载" sub="换个关键词试试" />
          )}
        </div>
      </div>

      {/* 批量删除条 */}
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
