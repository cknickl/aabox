# rtl8852be 5.10 → 6.1 backport — patches applied so far

Driver source: copied from Radxa kernel `stable-5.10-rock5` branch path
`drivers/net/wireless/rockchip_wlan/rtl8852be/` (30 MB, 874 files)
to `/aosp/rock5bp_a14/external/wifi_driver/rtl8852be/`.

Parent `external/wifi_driver/Makefile` patched to add:
```
CONFIG_RTL8852BE = m
export CONFIG_RTL8852BE
obj-$(CONFIG_RTL8852BE) += rtl8852be/
```

## Applied patches (2026-05-21 evening session)

1. **rtl8852be/Makefile** — added `EXTRA_CFLAGS += -Wno-error=fortify-source` and `-Wno-fortify-source` after the `-I$(src)/include` line. Silences clang sscanf buffer-size errors.

2. **rtl8852be/os_dep/linux/os_intfs.c:294** — `netif_napi_add(dev, &adapter->napi, rtw_recv_napi_poll, RTL_NAPI_WEIGHT)` → `netif_napi_add_weight(...)`. (6.1 removed weight arg from netif_napi_add; use _weight variant to preserve old behavior.)

3. **rtl8852be/os_dep/osdep_service_linux.c:850** — `prandom_u32()` → `get_random_u32()`. (Renamed in 6.1.)

4. **All `pci_dma_*` and `PCI_DMA_*` constants** — bulk sed across the entire driver:
   - `pci_map_single(pdev, ...)` → `dma_map_single(&pdev->dev, ...)`
   - `pci_unmap_single(pdev, ...)` → `dma_unmap_single(&pdev->dev, ...)`
   - `pci_dma_sync_single_for_cpu(pdev, ...)` → `dma_sync_single_for_cpu(&pdev->dev, ...)`
   - `pci_dma_sync_single_for_device(pdev, ...)` → `dma_sync_single_for_device(&pdev->dev, ...)`
   - `PCI_DMA_FROMDEVICE` → `DMA_FROM_DEVICE`
   - `PCI_DMA_TODEVICE` → `DMA_TO_DEVICE`
   - `PCI_DMA_BIDIRECTIONAL` → `DMA_BIDIRECTIONAL`
   - `PCI_DMA_NONE` → `DMA_NONE`

## Remaining errors to fix (NEXT SESSION)

These need ~3-5 hours of careful work:

1. **`os_dep/linux/pci_intf.c:513-514`** — `pci_set_dma_mask` and `pci_set_consistent_dma_mask` removed in 5.18. Replace both with single call:
   ```c
   dma_set_mask_and_coherent(&pdev->dev, DMA_BIT_MASK(32))  // or 64
   ```

2. **`os_dep/linux/ioctl_cfg80211.c`** — many cfg80211 function pointer signature changes for 6.x MLO (802.11be):
   - `add_key/del_key/get_key/set_default_key` gained `int link_id` parameter (insert after `wiphy/netdev/key_index`)
   - `start_radar_detection`, `change_bss`, `change_chan_state` (etc) gained `link_id`
   - Apply by adding `int link_id` to function defs (just ignore link_id in the function body for now — 8852BE is Wi-Fi 6, not 6E or 7, so MLO not relevant)

3. **`ioctl_cfg80211.c:1197`** — `cfg80211_roam_info.bssid` → `roam_info.links[0].bssid` (MLO restructuring)

4. **`ioctl_cfg80211.c` `wireless_dev.current_bss`** — removed in 6.x. Use `cfg80211_get_bss()` or wdev->links[0].client.current_bss.

5. **`os_dep/linux/wifi_regd.c:697`** — `REGULATORY_IGNORE_STALE_KICKOFF` removed. Just delete the flag reference.

6. **`core/rtw_wlan_util.c:2264, 2301`** — add `fallthrough;` annotations between switch case labels (Linux is now strict on case fall-through).

7. **`core/rtw_vht.c:1419`** — change `pnetwork->IEs != NULL` to `pnetwork->IE_length > 0` or similar (array compare to NULL is always-true tautology in 6.x clang).

## After patches compile, still TODO

- Firmware blob `rtw89/rtw8852b_fw.bin` — add to vendor/etc/firmware/ via PRODUCT_COPY_FILES. Available from linux-firmware project.
- Update DTS `wireless_wlan` node — currently `wifi_chip_type = "ap6275p"`. For Realtek PCIe-loaded driver, no chip_type needed (PCIe enumerates by VID/PID). Can delete the chip_type prop or change to "rtl8852be".
- Boot test: confirm `wlan0` appears in `ls /sys/class/net/` after rebuild + reflash.
