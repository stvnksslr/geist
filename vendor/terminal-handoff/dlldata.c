/* Proxy file list for geistHandoffProxy.dll: both handoff IDLs in one DLL.
   Hand-written because MIDL emits a dlldata.c per IDL and each names only its
   own interfaces.

   The proxy/stub CLSID is fixed here rather than MIDL's default (the first
   IID), so it can never collide with Windows Terminal's OpenConsoleProxy
   {3171DE52-...}. Keep in sync with PROXY_CLSID in src/handoff.rs. */
#define PROXY_CLSID_IS {0x4cdf6a34, 0x42c2, 0x488c, {0x84, 0xd2, 0x4b, 0xc3, 0xf5, 0x5f, 0x51, 0x9d}}

#include <rpcproxy.h>

EXTERN_PROXY_FILE(ITerminalHandoff)
EXTERN_PROXY_FILE(IConsoleHandoff)

PROXYFILE_LIST_START
    REFERENCE_PROXY_FILE(ITerminalHandoff),
    REFERENCE_PROXY_FILE(IConsoleHandoff),
PROXYFILE_LIST_END

DLLDATA_ROUTINES(aProxyFileList, GET_DLL_CLSID)
