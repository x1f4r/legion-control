import LegionControlCore

// The whole of the executable. Everything the app does lives in the library beside it, which is
// what lets the transport, the config and the updater be exercised without a window on screen.
LegionControlMain.main()
