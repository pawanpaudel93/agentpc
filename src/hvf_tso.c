// Loaded into QEMU (DYLD_INSERT_LIBRARIES) for x86apps VMs: turns on Apple Silicon's
// hardware TSO, x86's memory ordering, for every vCPU QEMU creates by setting ACTLR_EL1
// bit 1 (macOS 15+). FEX can then skip emulating that ordering. agentpc reads the log
// line below from qemu.log to decide whether it's safe to tell FEX so.
#include <Hypervisor/Hypervisor.h>
#include <stdio.h>

// HV_SYS_REG_ACTLR_EL1, which older SDKs don't name.
#define ACTLR_EL1 ((hv_sys_reg_t)0xc081)
#define ACTLR_EL1_TSO (1ULL << 1)

static hv_return_t tso_vcpu_create(hv_vcpu_t *vcpu, hv_vcpu_exit_t **exit, hv_vcpu_config_t config) {
    hv_return_t r = hv_vcpu_create(vcpu, exit, config);
    if (r == HV_SUCCESS) {
        uint64_t v = 0;
        int on = hv_vcpu_get_sys_reg(*vcpu, ACTLR_EL1, &v) == HV_SUCCESS &&
                 hv_vcpu_set_sys_reg(*vcpu, ACTLR_EL1, v | ACTLR_EL1_TSO) == HV_SUCCESS;
        fprintf(stderr, "agentpc-tso: vcpu %llu %s\n", (unsigned long long)*vcpu, on ? "TSO on" : "TSO unavailable");
    }
    return r;
}

__attribute__((used)) static const struct {
    const void *replacement, *replacee;
} interposers[] __attribute__((section("__DATA,__interpose"))) = {
    {(const void *)tso_vcpu_create, (const void *)hv_vcpu_create},
};
