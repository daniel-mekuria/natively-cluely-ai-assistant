// Shared between the spike-A KMD (driver.c) and its user-mode client (client.c).
// Keep in sync: the driver and the client must compute the identical IOCTL code.
#pragma once

// CTL_CODE / FILE_DEVICE_* / METHOD_* / buffer-access flags come from <wdm.h> in kernel
// mode (already pulled in by <ntddk.h> before this header in driver.c) and from
// <winioctl.h> in user mode (the client includes <windows.h> first). Do NOT drag the
// user-mode winioctl.h into the kernel compile — it carries kernel-incompatible content.
#ifndef _NTDDK_
#include <winioctl.h>
#endif

// The driver creates a single non-PnP device (\\Device\NativelyOverlay) with
// IoCreateDevice and a symbolic link \\.\NativelyOverlay.
//
// IOCTL_NOV_ECHO: METHOD_BUFFERED, reads a ULONG, writes back ULONG+1. It exists
// only to prove the build/sign/load/IPC pipeline end to end — it is NOT the
// real overlay protocol (Phase B defines the texture/rect/plane IOCTLs).
#define IOCTL_NOV_ECHO CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_BUFFERED, FILE_READ_DATA)