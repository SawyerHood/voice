import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Copy, CornerDownLeft, Trash2, RefreshCw, FileText } from "lucide-react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { ScrollArea } from "@/components/ui/scroll-area";
import {
  formatDuration,
  formatHistoryTimestamp,
  formatLanguageCode,
  formatProvider,
  type HistoryEntry,
} from "./historyUtils";

const HISTORY_PAGE_SIZE = 25;

type EntryAction = "copy" | "insert" | "delete";
type ActiveEntryAction = { id: string; type: EntryAction } | null;

function toErrorMessage(error: unknown, fallbackMessage: string): string {
  if (typeof error === "string" && error.trim()) return error;
  if (error instanceof Error && error.message.trim()) return error.message;
  return fallbackMessage;
}

type HistoryPanelProps = {
  refreshSignal?: number;
};

function HistoryPanel({ refreshSignal = 0 }: HistoryPanelProps) {
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [offset, setOffset] = useState(0);
  const [hasMore, setHasMore] = useState(true);
  const [isLoading, setIsLoading] = useState(false);
  const [isClearingAll, setIsClearingAll] = useState(false);
  const [activeAction, setActiveAction] = useState<ActiveEntryAction>(null);
  const [loadError, setLoadError] = useState("");
  const [actionError, setActionError] = useState("");
  const [actionNotice, setActionNotice] = useState("");
  const previousRefreshSignal = useRef(refreshSignal);

  const loadEntries = useCallback(async (nextOffset: number, replace: boolean) => {
    setIsLoading(true);
    setLoadError("");

    try {
      const page = await invoke<HistoryEntry[]>("list_history", {
        limit: HISTORY_PAGE_SIZE,
        offset: nextOffset,
      });

      setEntries((existingEntries) => {
        if (replace) return page;
        const existingIds = new Set(existingEntries.map((entry) => entry.id));
        const uniquePageEntries = page.filter((entry) => !existingIds.has(entry.id));
        return [...existingEntries, ...uniquePageEntries];
      });
      setOffset(nextOffset + page.length);
      setHasMore(page.length === HISTORY_PAGE_SIZE);
    } catch (error) {
      setLoadError(toErrorMessage(error, "Failed to load transcript history."));
    } finally {
      setIsLoading(false);
    }
  }, []);

  const refreshHistory = useCallback(async () => {
    await loadEntries(0, true);
  }, [loadEntries]);

  useEffect(() => {
    void refreshHistory();
  }, [refreshHistory]);

  useEffect(() => {
    if (previousRefreshSignal.current === refreshSignal) return;
    previousRefreshSignal.current = refreshSignal;
    void refreshHistory();
  }, [refreshHistory, refreshSignal]);

  const runEntryAction = useCallback(
    async (
      entryId: string,
      actionType: EntryAction,
      work: () => Promise<void>,
      successMessage: string
    ) => {
      setActionError("");
      setActionNotice("");
      setActiveAction({ id: entryId, type: actionType });

      try {
        await work();
        setActionNotice(successMessage);
      } catch (error) {
        setActionError(toErrorMessage(error, "Unable to complete history action."));
      } finally {
        setActiveAction(null);
      }
    },
    []
  );

  const onCopy = useCallback(
    (entry: HistoryEntry) => {
      void runEntryAction(
        entry.id,
        "copy",
        () => invoke("copy_to_clipboard", { text: entry.text }),
        "Transcript copied to clipboard."
      );
    },
    [runEntryAction]
  );

  const onReinsert = useCallback(
    (entry: HistoryEntry) => {
      void runEntryAction(
        entry.id,
        "insert",
        () => invoke("insert_text", { text: entry.text }),
        "Transcript re-inserted into the focused app."
      );
    },
    [runEntryAction]
  );

  const onDelete = useCallback(
    (entry: HistoryEntry) => {
      if (!window.confirm("Delete this transcript entry?")) return;

      void runEntryAction(
        entry.id,
        "delete",
        async () => {
          const deleted = await invoke<boolean>("delete_history_entry", { id: entry.id });
          if (!deleted) throw new Error("That entry was already deleted.");
          await refreshHistory();
        },
        "Transcript deleted."
      );
    },
    [refreshHistory, runEntryAction]
  );

  const onLoadMore = useCallback(() => {
    if (isLoading || !hasMore) return;
    void loadEntries(offset, false);
  }, [hasMore, isLoading, loadEntries, offset]);

  const onClearAll = useCallback(() => {
    if (!entries.length || isClearingAll) return;
    if (!window.confirm("Clear all transcript history? This cannot be undone.")) return;

    void (async () => {
      setIsClearingAll(true);
      setActionError("");
      setActionNotice("");

      try {
        await invoke("clear_history");
        setEntries([]);
        setOffset(0);
        setHasMore(false);
        setActionNotice("History cleared.");
      } catch (error) {
        setActionError(toErrorMessage(error, "Failed to clear transcript history."));
      } finally {
        setIsClearingAll(false);
      }
    })();
  }, [entries.length, isClearingAll]);

  return (
    <div className="space-y-3">
      {/* Toolbar */}
      <div className="flex items-center justify-between">
        <p className="text-sm font-semibold">Recent transcripts</p>
        <div className="flex gap-1.5">
          <Button
            variant="ghost"
            size="xs"
            onClick={() => void refreshHistory()}
            disabled={isLoading || isClearingAll}
          >
            <RefreshCw className="size-3" />
            Refresh
          </Button>
          <Button
            variant="ghost"
            size="xs"
            className="text-destructive hover:text-destructive"
            onClick={onClearAll}
            disabled={!entries.length || isLoading || isClearingAll}
          >
            <Trash2 className="size-3" />
            {isClearingAll ? "Clearing..." : "Clear All"}
          </Button>
        </div>
      </div>

      {/* Error / Notice */}
      {loadError && (
        <Alert variant="destructive" className="py-2">
          <AlertDescription className="text-xs">{loadError}</AlertDescription>
        </Alert>
      )}
      {actionError && (
        <Alert variant="destructive" className="py-2">
          <AlertDescription className="text-xs">{actionError}</AlertDescription>
        </Alert>
      )}
      {actionNotice && (
        <Alert className="border-emerald-500/30 bg-emerald-50/50 py-2 dark:bg-emerald-950/20">
          <AlertDescription className="text-xs text-emerald-700 dark:text-emerald-400">
            {actionNotice}
          </AlertDescription>
        </Alert>
      )}

      {/* Entry List */}
      <ScrollArea className="max-h-[calc(100vh-220px)]">
        <div className="space-y-2 pr-2">
          {!isLoading && entries.length === 0 && (
            <Card className="border-dashed">
              <CardContent className="flex flex-col items-center justify-center py-10 text-center">
                <FileText className="mb-3 size-8 text-muted-foreground/50" />
                <p className="text-sm font-medium text-muted-foreground">No transcripts yet</p>
                <p className="mt-1 text-xs text-muted-foreground/70">
                  Press your hotkey to record — transcripts will appear here.
                </p>
              </CardContent>
            </Card>
          )}

          {entries.map((entry) => {
            const entryActionActive = activeAction?.id === entry.id;
            const entryActionsDisabled = entryActionActive || isClearingAll;

            return (
              <Card key={entry.id} className="group transition-shadow hover:shadow-md">
                <CardContent className="space-y-2 py-3">
                  {/* Transcript text */}
                  <p className="line-clamp-3 text-sm leading-relaxed break-words">
                    {entry.text}
                  </p>

                  {/* Metadata badges */}
                  <div className="flex flex-wrap gap-1.5">
                    <Badge variant="secondary" className="text-[10px] px-1.5 py-0 font-normal">
                      {formatHistoryTimestamp(entry.timestamp)}
                    </Badge>
                    <Badge variant="secondary" className="text-[10px] px-1.5 py-0 font-normal">
                      {formatDuration(entry.durationSecs)}
                    </Badge>
                    <Badge variant="secondary" className="text-[10px] px-1.5 py-0 font-normal">
                      {formatLanguageCode(entry.language)}
                    </Badge>
                    <Badge variant="outline" className="text-[10px] px-1.5 py-0 font-normal tracking-wide">
                      {formatProvider(entry.provider)}
                    </Badge>
                  </div>

                  {/* Action buttons — show on hover */}
                  <div className="flex gap-1.5 opacity-0 transition-opacity group-hover:opacity-100 group-focus-within:opacity-100">
                    <Button
                      variant="outline"
                      size="xs"
                      onClick={() => onCopy(entry)}
                      disabled={entryActionsDisabled}
                    >
                      <Copy className="size-3" />
                      {entryActionActive && activeAction?.type === "copy" ? "Copying..." : "Copy"}
                    </Button>
                    <Button
                      variant="outline"
                      size="xs"
                      onClick={() => onReinsert(entry)}
                      disabled={entryActionsDisabled}
                    >
                      <CornerDownLeft className="size-3" />
                      {entryActionActive && activeAction?.type === "insert"
                        ? "Re-inserting..."
                        : "Re-insert"}
                    </Button>
                    <Button
                      variant="outline"
                      size="xs"
                      className="text-destructive hover:text-destructive"
                      onClick={() => onDelete(entry)}
                      disabled={entryActionsDisabled}
                    >
                      <Trash2 className="size-3" />
                      {entryActionActive && activeAction?.type === "delete"
                        ? "Deleting..."
                        : "Delete"}
                    </Button>
                  </div>
                </CardContent>
              </Card>
            );
          })}
        </div>
      </ScrollArea>

      {/* Loading / Load More */}
      {isLoading && (
        <p className="text-center text-xs text-muted-foreground">Loading history...</p>
      )}

      {!isLoading && hasMore && entries.length > 0 && (
        <div className="flex justify-center">
          <Button variant="outline" size="sm" onClick={onLoadMore}>
            Load More
          </Button>
        </div>
      )}
    </div>
  );
}

export default HistoryPanel;
