#include "bindings/bindings.h"

extern "C" void legion_install_ios_immersive_bridge(void);

int main(int argc, char * argv[]) {
	legion_install_ios_immersive_bridge();
	ffi::start_app();
	return 0;
}
