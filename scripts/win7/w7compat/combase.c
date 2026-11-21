/* Пустая DLL: всё содержимое — переадресация экспорта в combase.def. */
#include <windows.h>
BOOL WINAPI DllMain(HINSTANCE h, DWORD r, LPVOID p) { (void)h; (void)r; (void)p; return TRUE; }
