/* api-ms-win-core-winrt-error-l1-1-0.dll для Windows 7.
 * RoOriginateErrorW лишь прикладывает к ошибке текст для отладчика (Win8+).
 * На Win7 такого механизма нет: отвечаем «не приложили» — так делает и сама
 * Windows, когда отладочные сведения выключены. */
#include <windows.h>
BOOL WINAPI DllMain(HINSTANCE h, DWORD r, LPVOID p) { (void)h; (void)r; (void)p; return TRUE; }
__declspec(dllexport) BOOL WINAPI RoOriginateErrorW(HRESULT error, UINT32 length, const WCHAR *message) {
    (void)error; (void)length; (void)message;
    return FALSE;
}
