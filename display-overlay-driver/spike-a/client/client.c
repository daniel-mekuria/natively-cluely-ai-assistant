// Phase A spike client: open the driver's device and round-trip IOCTL_NOV_ECHO.
// Exit 0 = the driver is loaded and answered correctly (input+1).

#include <windows.h>
#include <stdio.h>
#include "../overlay_ioctl.h"

int main(void)
{
    HANDLE h = CreateFileW(L"\\\\.\\NativelyOverlay", GENERIC_READ | GENERIC_WRITE,
                           0, NULL, OPEN_EXISTING, 0, NULL);
    if (h == INVALID_HANDLE_VALUE) {
        printf("open \\\\.\\NativelyOverlay failed: %lu (is the driver loaded?)\n",
               GetLastError());
        return 1;
    }

    DWORD in = 0x1234, out = 0;
    DWORD returned = 0;
    BOOL ok = DeviceIoControl(h, IOCTL_NOV_ECHO, &in, sizeof(in),
                              &out, sizeof(out), &returned, NULL);
    CloseHandle(h);

    if (!ok) {
        printf("DeviceIoControl IOCTL_NOV_ECHO failed: %lu\n", GetLastError());
        return 2;
    }

    int good = (out == in + 1);
    printf("sent 0x%08lX, got 0x%08lX -> %s\n", in, out,
           good ? "ROUND-TRIP OK (driver pipeline works)" : "MISMATCH");
    return good ? 0 : 3;
}