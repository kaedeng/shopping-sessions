#!/usr/bin/env python3
"""
Reads an `SS_PROBE=machine` run's score as the machine under it (`machine` in src/probe.rs):
the run slept 40 ms times one more than the probe's code, so the score minus the usual time
gives the code, and the code the facts. Prints the facts and the instance types, sizes and
CPUs in the lists below that agree with them. Standard library only.

Usage: decode_machine.py SCORE [BASE]   seconds; BASE is the usual time, 0.5922 by default
       decode_machine.py --code N        the facts of code N
       decode_machine.py --encode VENDOR PRODUCT CPUS MEMTOTAL_KB "MODEL NAME" HYPERVISOR(0/1)
                                         the code a machine gives, as the probe computes it
       decode_machine.py --collisions    the names in the lists one code cannot tell apart,
                                         and a check that every name decodes back to itself
"""
import sys
import zlib
from collections import defaultdict

STEP = 0.040
BASE = 0.5922
# The blocks, in order: (name, size). Mirrors MACHINE_BLOCKS in src/probe.rs.
BLOCKS = [("unreadable", 24), ("amazon", 108), ("microsoft", 18), ("google", 24), ("virtual", 43), ("bare", 72)]
PRODUCTS, VENDORS = 3, 43

CPUS6 = ["at most 8", "9-16", "17-24", "25-36", "37-48", "more than 48"]
CPUS3 = ["at most 8", "9-36", "more than 36"]
MEMORY = ["under 3 GiB", "3-6 GiB", "6 GiB or more"]
GHZ = ["", "under 2.45", "2.5", "2.6", "2.7-2.8", "2.9", "3.0", "3.1", "3.2-3.4", "3.5-3.6", "3.7-3.8", "3.9 and over"]
GHZ_TOP = [0, 245, 255, 265, 285, 295, 305, 315, 345, 365, 385, 10**9]  # class i: centi-GHz below GHZ_TOP[i]


def cpu_clock(ghz, kind="a CPU"):
    return f"{kind} with no clock in its name" if ghz == 0 else f"{kind} at {GHZ[ghz]} GHz"


def crc(text):
    return zlib.crc32(text.encode()) & 0xFFFFFFFF


def cpus6(n):
    return next(i for i, top in enumerate([8, 16, 24, 36, 48, 10**9]) if n <= top)


def cpus3(n):
    return next(i for i, top in enumerate([8, 36, 10**9]) if n <= top)


def memory_class(kb, n):
    gib = 1 << 20
    return 0 if n == 0 or kb < 3 * gib * n else 1 if kb < 6 * gib * n else 2


def ghz_class(centi):
    return 0 if centi == 0 else next(i for i in range(1, 12) if centi < GHZ_TOP[i])


def centi_ghz(model):
    if "@" not in model:
        return 0
    clock = model.split("@", 1)[1].strip()
    if not clock.endswith("GHz"):
        return 0
    try:
        return round(float(clock[:-3].strip()) * 100)
    except ValueError:
        return 0


def encode(vendor, product, cpus, memory_kb, model, hypervisor):
    """The probe's code, computed as src/probe.rs computes it."""
    vendor, product, model = vendor.strip(), product.strip(), model.strip()
    c6, c3, mem = cpus6(cpus), cpus3(cpus), memory_class(memory_kb, cpus)
    ghz = ghz_class(centi_ghz(model))
    workstation = int("Core(TM)" in model or "Xeon(R) W-" in model)
    if not vendor:
        block, local = 0, hypervisor + 2 * ghz
    elif vendor.startswith("Amazon"):
        fast = int(ghz == 10)
        block, local = 1, crc(product) % PRODUCTS + PRODUCTS * (c6 + 6 * (mem + 3 * fast))
    elif vendor.startswith("Microsoft"):
        block, local = 2, c6 + 6 * mem
    elif vendor.startswith("Google"):
        block, local = 3, c6 + 6 * (0 if ghz == 7 else 1 + mem)
    elif hypervisor:
        block, local = 4, crc(vendor) % VENDORS
    else:
        block, local = 5, c3 + 3 * (ghz + 12 * workstation)
    return sum(size for _, size in BLOCKS[:block]) + local


# AWS instance types of 8 vCPUs and more whose CPU can be a Cascade Lake (the scorer's):
# family -> CPU (the part and its nominal GHz, as in its name) and size -> (vCPUs, GiB).
C5 = {"2xlarge": (8, 16), "4xlarge": (16, 32), "9xlarge": (36, 72), "12xlarge": (48, 96), "18xlarge": (72, 144), "24xlarge": (96, 192), "metal": (96, 192)}
M5 = {"2xlarge": (8, 32), "4xlarge": (16, 64), "8xlarge": (32, 128), "12xlarge": (48, 192), "16xlarge": (64, 256), "24xlarge": (96, 384), "metal": (96, 384)}
R5 = {size: (n, 2 * gib) for size, (n, gib) in M5.items()}
AWS_FAMILIES = {
    "c5": ("8275CL 3.0 GHz", C5), "c5d": ("8275CL 3.0 GHz", C5),
    "m5": ("8259CL 2.5 GHz", M5), "m5d": ("8259CL 2.5 GHz", M5), "m5n": ("8259CL 2.5 GHz", M5), "m5dn": ("8259CL 2.5 GHz", M5),
    "m5zn": ("8252C 3.8 GHz", {"2xlarge": (8, 32), "3xlarge": (12, 48), "6xlarge": (24, 96), "12xlarge": (48, 192), "metal": (48, 192)}),
    "r5": ("8259CL 2.5 GHz", R5), "r5d": ("8259CL 2.5 GHz", R5), "r5n": ("8259CL 2.5 GHz", R5), "r5dn": ("8259CL 2.5 GHz", R5), "r5b": ("8259CL 2.5 GHz", R5),
    "x2iezn": ("8252C 3.8 GHz", {"2xlarge": (8, 256), "4xlarge": (16, 512), "6xlarge": (24, 768), "8xlarge": (32, 1024), "12xlarge": (48, 1536), "metal": (48, 1536)}),
    "d3": ("8259CL 2.5 GHz", {"2xlarge": (8, 64), "4xlarge": (16, 128), "8xlarge": (32, 256)}),
    "d3en": ("8259CL 2.5 GHz", {"2xlarge": (8, 32), "4xlarge": (16, 64), "6xlarge": (24, 96), "8xlarge": (32, 128), "12xlarge": (48, 192)}),
    "g4dn": ("8259CL 2.5 GHz", {"2xlarge": (8, 32), "4xlarge": (16, 64), "8xlarge": (32, 128), "12xlarge": (48, 192), "16xlarge": (64, 256), "metal": (96, 384)}),
    "inf1": ("8275CL 3.0 GHz", {"2xlarge": (8, 16), "6xlarge": (24, 48), "24xlarge": (96, 192)}),
    "i3en": ("8175M/8259CL 2.5 GHz", {"2xlarge": (8, 64), "3xlarge": (12, 96), "6xlarge": (24, 192), "12xlarge": (48, 384), "24xlarge": (96, 768), "metal": (96, 768)}),
    "vt1": ("8259CL 2.5 GHz", {"3xlarge": (12, 24), "6xlarge": (24, 48), "24xlarge": (96, 192)}),
    "t3": ("8175M/8259CL 2.5 GHz", {"2xlarge": (8, 32)}),
    "p4d": ("8275CL 3.0 GHz", {"24xlarge": (96, 1152)}), "p4de": ("8275CL 3.0 GHz", {"24xlarge": (96, 1152)}),
    "dl1": ("8275CL 3.0 GHz", {"24xlarge": (96, 768)}),
}
AWS = {f"{family}.{size}": (n, gib, cpu) for family, (cpu, sizes) in AWS_FAMILIES.items() for size, (n, gib) in sizes.items()}

# Azure sizes with a Cascade Lake (8272CL 2.6 GHz; Fsv2 and Dv3/Ev3 also run on older parts).
# DMI shows none of them (the product is "Virtual Machine"): they are matched by shape.
AZURE = {}
for n in (8, 16, 32, 48, 64, 72):
    AZURE[f"F{n}s_v2"] = (n, 2 * n)
for n in (8, 16, 32, 48, 64):
    for suffix in ("s_v4", "ds_v4", "s_v3"):
        AZURE[f"D{n}{suffix}"] = (n, 4 * n)
for n in (8, 16, 20, 32, 48, 64):
    for suffix in ("s_v4", "ds_v4", "s_v3"):
        AZURE[f"E{n}{suffix}"] = (n, 504 if n == 64 else 8 * n)
AZURE.update({"M208s_v2": (208, 2850), "M208ms_v2": (208, 5700), "M416s_v2": (416, 5700), "M416ms_v2": (416, 11400)})

# GCP machine types with a Cascade Lake, and the clock in their CPU's name. DMI says "Google
# Compute Engine" for all: matched by shape and clock.
GCP = {}
for n in (8, 16, 30, 60):
    GCP[f"c2-standard-{n}"] = (n, 4 * n, 310)
for n in (8, 16, 32, 48, 64, 80, 96, 128):
    GCP[f"n2-standard-{n}"] = (n, 4 * n, 280)
    GCP[f"n2-highmem-{n}"] = (n, 864 if n == 128 else 8 * n, 280)
    if n < 128:
        GCP[f"n2-highcpu-{n}"] = (n, n, 280)
for n in (8, 16, 32):
    GCP[f"e2-standard-{n}"] = (n, 4 * n, 220)
    GCP[f"e2-highcpu-{n}"] = (n, n, 220)
for n in (8, 16):
    GCP[f"e2-highmem-{n}"] = (n, 8 * n, 220)
GCP.update({"a2-highgpu-1g": (12, 85, 220), "a2-highgpu-2g": (24, 170, 220), "a2-highgpu-4g": (48, 340, 220), "a2-highgpu-8g": (96, 680, 220), "a2-megagpu-16g": (96, 1360, 220),
            "m2-ultramem-208": (208, 5888, 250), "m2-megamem-416": (416, 5888, 250), "m2-ultramem-416": (416, 11776, 250)})

# sys_vendor of virtual machines and hosting companies (with the hypervisor flag).
VIRTUAL = ["QEMU", "Xen", "VMware, Inc.", "innotek GmbH", "Oracle Corporation", "Red Hat", "OpenStack Foundation", "oVirt", "Nutanix", "Bochs",
           "Parallels Software International Inc.", "Parallels International GmbH.", "Apple Inc.", "KubeVirt", "Cloud Hypervisor",
           "DigitalOcean", "Hetzner", "Vultr", "Linode", "Akamai", "Alibaba Cloud", "Tencent Cloud", "Huawei Cloud", "Scaleway", "Exoscale",
           "UpCloud", "IBM", "OVH", "Equinix"]

# Cascade Lake and Cooper Lake CPUs: name, nominal GHz, cores, one socket only.
SKUS = [
    ("Core i9-10900X", 3.7, 10, True), ("Core i9-10920X", 3.5, 12, True), ("Core i9-10940X", 3.3, 14, True), ("Core i9-10980XE", 3.0, 18, True),
    ("Xeon W-2223", 3.6, 4, True), ("Xeon W-2225", 4.1, 4, True), ("Xeon W-2235", 3.8, 6, True), ("Xeon W-2245", 3.9, 8, True),
    ("Xeon W-2255", 3.7, 10, True), ("Xeon W-2265", 3.5, 12, True), ("Xeon W-2275", 3.3, 14, True), ("Xeon W-2295", 3.0, 18, True),
    ("Xeon W-3223", 3.5, 8, True), ("Xeon W-3225", 3.7, 8, True), ("Xeon W-3235", 3.3, 12, True), ("Xeon W-3245", 3.2, 16, True),
    ("Xeon W-3265", 2.7, 24, True), ("Xeon W-3275", 2.5, 28, True),
    ("Platinum 8280", 2.7, 28, False), ("Platinum 8276", 2.2, 28, False), ("Platinum 8270", 2.7, 26, False), ("Platinum 8268", 2.9, 24, False),
    ("Platinum 8260", 2.4, 24, False), ("Platinum 8256", 3.8, 4, False), ("Platinum 8253", 2.2, 16, False), ("Platinum 9282", 2.6, 56, False),
    ("Platinum 9242", 2.3, 48, False), ("Platinum 8275CL (AWS)", 3.0, 24, False), ("Platinum 8259CL (AWS)", 2.5, 24, False),
    ("Platinum 8252C (AWS)", 3.8, 12, False), ("Platinum 8272CL (Azure)", 2.6, 26, False),
    ("Gold 6258R", 2.7, 28, False), ("Gold 6248R", 3.0, 24, False), ("Gold 6246R", 3.4, 16, False), ("Gold 6242R", 3.1, 20, False),
    ("Gold 6240R", 2.4, 24, False), ("Gold 6238R", 2.2, 28, False), ("Gold 6230R", 2.1, 26, False), ("Gold 6226R", 2.9, 16, False),
    ("Gold 6208U", 2.9, 16, False), ("Gold 6210U", 2.5, 20, False), ("Gold 6212U", 2.4, 24, False),
    ("Gold 6254", 3.1, 18, False), ("Gold 6252", 2.1, 24, False), ("Gold 6248", 2.5, 20, False), ("Gold 6246", 3.3, 12, False),
    ("Gold 6244", 3.6, 8, False), ("Gold 6242", 2.8, 16, False), ("Gold 6240", 2.6, 18, False), ("Gold 6238", 2.1, 22, False),
    ("Gold 6234", 3.3, 8, False), ("Gold 6230", 2.1, 20, False), ("Gold 6226", 2.7, 12, False),
    ("Gold 5222", 3.8, 4, False), ("Gold 5220", 2.2, 18, False), ("Gold 5220R", 2.2, 24, False), ("Gold 5218", 2.3, 16, False),
    ("Gold 5218R", 2.1, 20, False), ("Gold 5217", 3.0, 8, False), ("Gold 5215", 2.5, 10, False),
    ("Silver 4216", 2.1, 16, False), ("Silver 4215R", 3.2, 8, False), ("Silver 4215", 2.5, 8, False), ("Silver 4214R", 2.4, 12, False),
    ("Silver 4214", 2.2, 12, False), ("Silver 4210R", 2.4, 10, False), ("Silver 4210", 2.2, 10, False), ("Silver 4208", 2.1, 8, False),
    ("Platinum 8380H (Cooper Lake)", 2.9, 28, False), ("Platinum 8376H (Cooper Lake)", 2.6, 28, False),
    ("Platinum 8354H (Cooper Lake)", 3.1, 18, False), ("Gold 6348H (Cooper Lake)", 2.3, 24, False), ("Gold 6328H (Cooper Lake)", 2.8, 16, False),
]


def skus(ghz, workstation, count_class=None):
    """The CPUs of a clock class and kind, with the host CPU counts (2 threads a core, one or
    two sockets) that fall in `count_class` of CPUS3 when it is given."""
    found = []
    for name, nominal, cores, one_socket in SKUS:
        if ghz_class(round(nominal * 100)) != ghz or one_socket != bool(workstation):
            continue
        counts = [2 * cores] if one_socket else [2 * cores, 4 * cores]
        if count_class is not None:
            counts = [n for n in counts if cpus3(n) == count_class]
            if not counts:
                continue
        found.append(f"{name} ({' or '.join(str(n) for n in counts)} CPUs)")
    return found


def split(code):
    """(block name, local code), or None past the last block."""
    for name, size in BLOCKS:
        if code < size:
            return name, code
        code -= size
    return None


def gib_kb(gib):
    """MemTotal for a machine of `gib` GiB: the kernel keeps a few percent."""
    return int(gib * 0.97 * (1 << 20))


def aws_model(cpu):
    """The CPU's name as /proc/cpuinfo shows it, from AWS_FAMILIES' "8275CL 3.0 GHz"."""
    return f"Intel(R) Xeon(R) Platinum {cpu.split()[0]} CPU @ {float(cpu.split()[1]):.2f}GHz"


def aws_matches(local):
    residue, rest = local % PRODUCTS, local // PRODUCTS
    c6, rest = rest % 6, rest // 6
    mem, fast = rest % 3, rest // 3
    return [name for name, (n, gib, cpu) in AWS.items()
            if crc(name) % PRODUCTS == residue and cpus6(n) == c6 and memory_class(gib_kb(gib), n) == mem and (ghz_class(centi_ghz(aws_model(cpu))) == 10) == bool(fast)]


def azure_matches(local):
    c6, mem = local % 6, local // 6
    return [name for name, (n, gib) in AZURE.items() if cpus6(n) == c6 and memory_class(gib_kb(gib), n) == mem]


def gcp_matches(local):
    c6, kind = local % 6, local // 6
    if kind == 0:
        return [name for name, (n, _, centi) in GCP.items() if centi == 310 and cpus6(n) == c6]
    return [name for name, (n, gib, centi) in GCP.items() if centi != 310 and cpus6(n) == c6 and memory_class(gib_kb(gib), n) == kind - 1]


def describe(code):
    """A one-line summary and the detail lines of a code."""
    where = split(code)
    if where is None:
        return "past the last block (the base is off, or this is not a machine probe)", []
    block, local = where
    if block == "unreadable":
        hyp, ghz = local % 2, local // 2
        kind = "a virtual machine (WSL2, a microVM)" if hyp else "no hypervisor flag (bare metal without DMI)"
        return f"DMI unreadable, {kind}, {cpu_clock(ghz)}", ["CPUs of that clock: " + (", ".join(skus(ghz, 1) + skus(ghz, 0)) or "none in the list")]
    if block == "amazon":
        residue, rest = local % PRODUCTS, local // PRODUCTS
        c6, rest = rest % 6, rest // 6
        mem, fast = rest % 3, rest // 3
        matches = aws_matches(local)
        named = ", ".join(f"{name}, {AWS[name][0]} CPUs, {AWS[name][1]} GiB ({AWS[name][2]})" for name in matches)
        head = f"Amazon EC2, {named if len(matches) == 1 else 'one of: ' + named if matches else 'a type not in the list'}"
        clock = "a CPU at 3.7-3.8 GHz (8252C)" if fast else "a CPU not at 3.7-3.8 GHz"
        return head, [f"host CPUs {CPUS6[c6]}, {MEMORY[mem]} a CPU, {clock}, crc32(product_name) % {PRODUCTS} = {residue}"]
    if block == "microsoft":
        c6, mem = local % 6, local // 6
        sizes = azure_matches(local)
        return f"Microsoft (Azure or Hyper-V), host CPUs {CPUS6[c6]}, {MEMORY[mem]} a CPU", ["Azure sizes of that shape: " + (", ".join(sizes) or "none in the list")]
    if block == "google":
        c6, kind = local % 6, local // 6
        types = gcp_matches(local)
        if kind == 0:
            return f"Google, a 3.1 GHz CPU (C2), host CPUs {CPUS6[c6]}", ["GCP types: " + (", ".join(types) or "none in the list")]
        mem = kind - 1
        return f"Google, not 3.1 GHz, host CPUs {CPUS6[c6]}, {MEMORY[mem]} a CPU", ["GCP types: " + (", ".join(types) or "none in the list")]
    if block == "virtual":
        names = [name for name in VIRTUAL if crc(name) % VENDORS == local]
        return f"a virtual machine, vendor {' or '.join(names) if names else 'not in the list'}", [f"crc32(sys_vendor) % {VENDORS} = {local}"]
    c3, rest = local % 3, local // 3
    ghz, workstation = rest % 12, rest // 12
    cpu = cpu_clock(ghz, "Core or Xeon W" if workstation else "Xeon Scalable or other")
    return f"bare metal (a readable vendor, no hypervisor flag), {cpu}, host CPUs {CPUS3[c3]}", ["CPUs that fit: " + (", ".join(skus(ghz, workstation, c3)) or "none in the list")]


def show(code):
    head, lines = describe(code)
    print(f"code {code}: {head}")
    for line in lines:
        print(f"  {line}")


def collisions():
    total = sum(size for _, size in BLOCKS)
    print(f"{total} codes, the longest sleep {total * STEP:.2f} s")
    groups = defaultdict(list)
    for name, (n, gib, cpu) in AWS.items():
        groups[encode("Amazon EC2", name, n, gib_kb(gib), aws_model(cpu), 1)].append(name)
    shared = [names for names in groups.values() if len(names) > 1]
    print(f"AWS: {len(AWS)} types, {sum(map(len, shared))} share a code with another:")
    for names in shared:
        print("  " + ", ".join(names))
    groups = defaultdict(list)
    for name in VIRTUAL:
        groups[encode(name, "", 8, 0, "", 1)].append(name)
    shared = [names for names in groups.values() if len(names) > 1]
    print(f"vendors: {len(VIRTUAL)}, {sum(map(len, shared))} share a code with another:")
    for names in shared:
        print("  " + ", ".join(names))
    # Every name decodes back to a list that holds it.
    bad = []
    for name, (n, gib, cpu) in AWS.items():
        block, local = split(encode("Amazon EC2", name, n, gib_kb(gib), aws_model(cpu), 1))
        if block != "amazon" or name not in aws_matches(local):
            bad.append(name)
    for name, (n, gib) in AZURE.items():
        block, local = split(encode("Microsoft Corporation", "Virtual Machine", n, gib_kb(gib), "Intel(R) Xeon(R) Platinum 8272CL CPU @ 2.60GHz", 1))
        if block != "microsoft" or name not in azure_matches(local):
            bad.append(name)
    for name, (n, gib, centi) in GCP.items():
        block, local = split(encode("Google", "Google Compute Engine", n, gib_kb(gib), f"Intel(R) Xeon(R) CPU @ {centi / 100:.2f}GHz", 1))
        if block != "google" or name not in gcp_matches(local):
            bad.append(name)
    for name, nominal, cores, one_socket in SKUS:
        brand = "Intel(R) Core(TM) i9-X" if name.startswith("Core") else "Intel(R) Xeon(R) W-X" if name.startswith("Xeon W") else "Intel(R) Xeon(R) Gold X"
        model = f"{brand} CPU @ {nominal:.2f}GHz"
        block, local = split(encode("Supermicro", "Super Server", 2 * cores, 64 << 20, model, 0))
        found = " | ".join(describe(sum(size for _, size in BLOCKS[:5]) + local)[1]) if block == "bare" else ""
        block_u, local_u = split(encode("", "", 2 * cores, 64 << 20, model, 1))
        found_u = " | ".join(describe(local_u)[1]) if block_u == "unreadable" else ""
        if f"{name} (" not in found or f"{name} (" not in found_u:
            bad.append(name)
    print("round trip: " + ("every name decodes back into its own list" if not bad else "FAILED for " + ", ".join(bad)))


def main():
    args = sys.argv[1:]
    if not args or args[0] in ("-h", "--help"):
        print(__doc__.strip())
        return
    if args[0] == "--collisions":
        collisions()
        return
    if args[0] == "--code":
        show(int(args[1]))
        return
    if args[0] == "--encode":
        vendor, product, cpus, memory_kb, model, hypervisor = args[1:7]
        code = encode(vendor, product, int(cpus), int(memory_kb), model, int(hypervisor))
        print(f"code {code}: sleeps {(code + 1) * STEP:.2f} s, so a score of BASE + {(code + 1) * STEP:.2f} s ({BASE + (code + 1) * STEP:.4f} at the default base)")
        show(code)
        return
    score = float(args[0])
    base = float(args[1]) if len(args) > 1 else BASE
    steps = (score - base) / STEP
    nearest = round(steps)
    off = (steps - nearest) * STEP * 1e3
    print(f"score {score:.4f} s, base {base:.4f} s: {steps:.2f} steps of 40 ms ({off:+.1f} ms from a whole step)")
    if nearest <= 0:
        print("no sleep: the probe did not run (or the base is off)")
        return
    show(nearest - 1)
    if abs(off) > 12:
        neighbour = nearest + (1 if off > 0 else -1)
        if neighbour >= 1:
            print("more than 12 ms off a step, so maybe the neighbouring one:")
            show(neighbour - 1)


if __name__ == "__main__":
    main()
