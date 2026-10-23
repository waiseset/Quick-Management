import { useAddItemFlow } from "../addItem";
import { useDragList } from "../drag";
import { useApp } from "../store";
import { useContextMenu } from "./ContextMenu";
import { ItemCard, useItemClipboard } from "./ItemCard";

/**
 * 参考文档第 4 行以下：条目网格。
 * 卡片本身的单击 / 双击 / 右键菜单 / 拖拽都在 ItemCard 里（与「隐藏的元素」页共用），
 * 这里只负责网格容器（点空白取消选中、右键「添加 / 批量添加应用 / 粘贴」）。
 */
export function ItemGrid() {
  const { visibleItems, selectItem, reorderItems, clipboard } = useApp();
  const { open } = useContextMenu();
  const { paste } = useItemClipboard();
  // 主视图里新增的条目是普通条目
  const { startAdd, bulkAddApps, editor } = useAddItemFlow(false);

  const ids = visibleItems.map((item) => item.id);
  const drag = useDragList(ids, reorderItems);

  return (
    <>
      <div
        className="item-grid"
        onClick={(event) => {
          // 点在卡片上时由卡片自己处理选中，这里只负责「点空白处取消选中」
          if (event.target === event.currentTarget) selectItem(null);
        }}
        onContextMenu={(event) => {
          event.preventDefault();
          open({
            x: event.clientX,
            y: event.clientY,
            items: [
              { key: "add", label: "添加", onSelect: startAdd },
              { key: "bulk", label: "批量添加应用…", onSelect: () => void bulkAddApps() },
              {
                key: "paste",
                label: "粘贴",
                hint: clipboard
                  ? clipboard.items.length > 1
                    ? `${clipboard.items.length} 个条目`
                    : clipboard.items[0].name
                  : undefined,
                disabled: !clipboard,
                onSelect: paste,
              },
            ],
          });
        }}
      >
        {visibleItems.map((item) => (
          <ItemCard key={item.id} item={item} drag={drag} />
        ))}

        {visibleItems.length === 0 ? (
          <div className="item-empty">
            <p>这里还没有条目</p>
            <p className="item-empty-hint">右键空白处可以「添加」</p>
          </div>
        ) : null}
      </div>

      {editor}
    </>
  );
}
