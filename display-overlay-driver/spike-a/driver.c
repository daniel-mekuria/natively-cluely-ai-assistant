// Natively display-overlay KMD — Phase A spike.
//
// A minimal non-PnP WDM "kernel" driver that creates one device
// (\\Device\NativelyOverlay, symlinked to \\.\NativelyOverlay) and answers a
// single IOCTL (IOCTL_NOV_ECHO: returns input+1). A user-mode client opens the
// device and round-trips the IOCTL to prove the build / sign / load / IPC
// pipeline on this box. This is the PIPELINE probe — the overlay-plane
// compositing (Phase B) and the production driver (Phase C) build on it.

#include <ntddk.h>
#include "overlay_ioctl.h"

static PDEVICE_OBJECT g_Device = NULL;

static VOID DriverUnload(PDRIVER_OBJECT DriverObject)
{
    UNICODE_STRING sym;
    RtlInitUnicodeString(&sym, L"\\DosDevices\\NativelyOverlay");
    IoDeleteSymbolicLink(&sym);
    IoDeleteDevice(g_Device);
    g_Device = NULL;
}

static NTSTATUS DriverDispatch(PDEVICE_OBJECT DeviceObject, PIRP Irp)
{
    PIO_STACK_LOCATION sl = IoGetCurrentIrpStackLocation(Irp);
    (void)DeviceObject;

    switch (sl->MajorFunction) {
    case IRP_MJ_CREATE:
    case IRP_MJ_CLOSE:
    case IRP_MJ_CLEANUP:
        Irp->IoStatus.Status = STATUS_SUCCESS;
        Irp->IoStatus.Information = 0;
        break;

    case IRP_MJ_DEVICE_CONTROL: {
        NTSTATUS st = STATUS_SUCCESS;
        ULONG code   = sl->Parameters.DeviceIoControl.IoControlCode;
        ULONG inLen  = sl->Parameters.DeviceIoControl.InputBufferLength;
        ULONG outLen = sl->Parameters.DeviceIoControl.OutputBufferLength;

        if (code == IOCTL_NOV_ECHO) {
            // The client always sends METHOD_BUFFERED (as the IOCTL is defined); the
            // params struct exposes no TransferType, so trust SystemBuffer.
            if (inLen < sizeof(ULONG) || outLen < sizeof(ULONG)) {
                st = STATUS_INVALID_PARAMETER;
            } else {
                PULONG v = (PULONG)Irp->AssociatedIrp.SystemBuffer;
                *v = *v + 1;
                Irp->IoStatus.Information = sizeof(ULONG);
            }
        } else {
            st = STATUS_INVALID_DEVICE_REQUEST;
        }
        Irp->IoStatus.Status = st;
        break;
    }

    default:
        Irp->IoStatus.Status = STATUS_NOT_SUPPORTED;
        break;
    }

    IoCompleteRequest(Irp, IO_NO_INCREMENT);
    return Irp->IoStatus.Status;
}

NTSTATUS DriverEntry(PDRIVER_OBJECT DriverObject, PUNICODE_STRING RegistryPath)
{
    (void)RegistryPath;
    UNICODE_STRING devName, symLink;
    OBJECT_ATTRIBUTES oa;

    DriverObject->DriverUnload = DriverUnload;
    DriverObject->MajorFunction[IRP_MJ_CREATE]           = DriverDispatch;
    DriverObject->MajorFunction[IRP_MJ_CLOSE]            = DriverDispatch;
    DriverObject->MajorFunction[IRP_MJ_CLEANUP]          = DriverDispatch;
    DriverObject->MajorFunction[IRP_MJ_DEVICE_CONTROL]   = DriverDispatch;

    RtlInitUnicodeString(&devName, L"\\Device\\NativelyOverlay");
    InitializeObjectAttributes(&oa, &devName, OBJ_CASE_INSENSITIVE, NULL, NULL);

    NTSTATUS st = IoCreateDevice(DriverObject, 0, &devName,
                                 FILE_DEVICE_UNKNOWN, 0, FALSE, &g_Device);
    if (!NT_SUCCESS(st)) {
        return st;
    }

    RtlInitUnicodeString(&symLink, L"\\DosDevices\\NativelyOverlay");
    st = IoCreateSymbolicLink(&symLink, &devName);
    if (!NT_SUCCESS(st)) {
        IoDeleteDevice(g_Device);
        g_Device = NULL;
        return st;
    }

    return STATUS_SUCCESS;
}