#![feature(test)]
#![cfg(feature = "bpf_c")]

extern crate test;
#[macro_use]
extern crate solana_bpf_loader_program;

use byteorder::{ByteOrder, LittleEndian, WriteBytesExt};
use solana_bpf_loader_program::{
    create_vm, serialization::serialize_parameters, syscalls::register_syscalls, BpfError,
    ThisInstructionMeter,
};
use solana_measure::measure::Measure;
use solana_rbpf::vm::{Config, Executable, InstructionMeter, SyscallRegistry};
use solana_runtime::{
    bank::Bank,
    bank_client::BankClient,
    genesis_utils::{create_genesis_config, GenesisConfigInfo},
    loader_utils::load_program,
};
use solana_program_runtime::invoke_context::with_mock_invoke_context;
use solana_sdk::{
    bpf_loader,
    client::SyncClient,
    entrypoint::SUCCESS,
    instruction::{AccountMeta, Instruction},
    message::Message,
    process_instruction::InvokeContext,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};
use std::{env, fs::File, io::Read, mem, path::PathBuf, sync::Arc};
use test::Bencher;

/// BPF program file extension
const PLATFORM_FILE_EXTENSION_BPF: &str = "so";
/// Create a BPF program file name
fn create_bpf_path(name: &str) -> PathBuf {
    let mut pathbuf = {
        let current_exe = env::current_exe().unwrap();
        PathBuf::from(current_exe.parent().unwrap().parent().unwrap())
    };
    pathbuf.push("bpf/");
    pathbuf.push(name);
    pathbuf.set_extension(PLATFORM_FILE_EXTENSION_BPF);
    pathbuf
}

fn load_elf(name: &str) -> Result<Vec<u8>, std::io::Error> {
    let path = create_bpf_path(name);
    let mut file = File::open(&path).expect(&format!("Unable to open {:?}", path));
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    Ok(elf)
}

fn load_bpf_program(
    bank_client: &BankClient,
    loader_id: &Pubkey,
    payer_keypair: &Keypair,
    name: &str,
) -> Pubkey {
    let path = create_bpf_path(name);
    let mut file = File::open(path).unwrap();
    let mut elf = Vec::new();
    file.read_to_end(&mut elf).unwrap();
    load_program(bank_client, payer_keypair, loader_id, elf)
}

const ARMSTRONG_LIMIT: u64 = 500;
const ARMSTRONG_EXPECTED: u64 = 5;

#[bench]
fn bench_program_create_executable(bencher: &mut Bencher) {
    let elf = load_elf("bench_alu").unwrap();

    bencher.iter(|| {
        let _ = <dyn Executable<BpfError, ThisInstructionMeter>>::from_elf(
            &elf,
            None,
            Config::default(),
            SyscallRegistry::default(),
        )
        .unwrap();
    });
}

#[bench]
fn bench_program_alu(bencher: &mut Bencher) {
    let ns_per_s = 1000000000;
    let one_million = 1000000;
    let mut inner_iter = vec![];
    inner_iter
        .write_u64::<LittleEndian>(ARMSTRONG_LIMIT)
        .unwrap();
    inner_iter.write_u64::<LittleEndian>(0).unwrap();
    let elf = load_elf("bench_alu").unwrap();
    let loader_id = bpf_loader::id();
    with_mock_invoke_context(loader_id, 10000001, |invoke_context| {
        let mut executable = <dyn Executable<BpfError, ThisInstructionMeter>>::from_elf(
            &elf,
            None,
            Config::default(),
            register_syscalls(invoke_context).unwrap(),
        )
        .unwrap();
        executable.jit_compile().unwrap();
        let compute_meter = invoke_context.get_compute_meter();
        let mut instruction_meter = ThisInstructionMeter { compute_meter };
        let mut vm = create_vm(
            &loader_id,
            executable.as_ref(),
            &mut inner_iter,
            invoke_context,
            &[],
        )
        .unwrap();

        println!("Interpreted:");
        assert_eq!(
            SUCCESS,
            vm.execute_program_interpreted(&mut instruction_meter)
                .unwrap()
        );
        assert_eq!(ARMSTRONG_LIMIT, LittleEndian::read_u64(&inner_iter));
        assert_eq!(
            ARMSTRONG_EXPECTED,
            LittleEndian::read_u64(&inner_iter[mem::size_of::<u64>()..])
        );

        bencher.iter(|| {
            vm.execute_program_interpreted(&mut instruction_meter)
                .unwrap();
        });
        let instructions = vm.get_total_instruction_count();
        let summary = bencher.bench(|_bencher| {}).unwrap();
        println!("  {:?} instructions", instructions);
        println!("  {:?} ns/iter median", summary.median as u64);
        assert!(0f64 != summary.median);
        let mips = (instructions * (ns_per_s / summary.median as u64)) / one_million;
        println!("  {:?} MIPS", mips);
        println!("{{ \"type\": \"bench\", \"name\": \"bench_program_alu_interpreted_mips\", \"median\": {:?}, \"deviation\": 0 }}", mips);

        println!("JIT to native:");
        assert_eq!(
            SUCCESS,
            vm.execute_program_jit(&mut instruction_meter).unwrap()
        );
        assert_eq!(ARMSTRONG_LIMIT, LittleEndian::read_u64(&inner_iter));
        assert_eq!(
            ARMSTRONG_EXPECTED,
            LittleEndian::read_u64(&inner_iter[mem::size_of::<u64>()..])
        );

        bencher.iter(|| vm.execute_program_jit(&mut instruction_meter).unwrap());
        let summary = bencher.bench(|_bencher| {}).unwrap();
        println!("  {:?} instructions", instructions);
        println!("  {:?} ns/iter median", summary.median as u64);
        assert!(0f64 != summary.median);
        let mips = (instructions * (ns_per_s / summary.median as u64)) / one_million;
        println!("  {:?} MIPS", mips);
        println!("{{ \"type\": \"bench\", \"name\": \"bench_program_alu_jit_to_native_mips\", \"median\": {:?}, \"deviation\": 0 }}", mips);
    });
}

#[bench]
fn bench_program_execute_noop(bencher: &mut Bencher) {
    let GenesisConfigInfo {
        genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config(50);
    let mut bank = Bank::new_for_benches(&genesis_config);
    let (name, id, entrypoint) = solana_bpf_loader_program!();
    bank.add_builtin(&name, &id, entrypoint);
    let bank = Arc::new(bank);
    let bank_client = BankClient::new_shared(&bank);

    let invoke_program_id =
        load_bpf_program(&bank_client, &bpf_loader::id(), &mint_keypair, "noop");

    let mint_pubkey = mint_keypair.pubkey();
    let account_metas = vec![AccountMeta::new(mint_pubkey, true)];

    let instruction =
        Instruction::new_with_bincode(invoke_program_id, &[u8::MAX, 0, 0, 0], account_metas);
    let message = Message::new(&[instruction], Some(&mint_pubkey));

    bank_client
        .send_and_confirm_message(&[&mint_keypair], message.clone())
        .unwrap();

    bencher.iter(|| {
        bank.clear_signatures();
        bank_client
            .send_and_confirm_message(&[&mint_keypair], message.clone())
            .unwrap();
    });
}

#[bench]
fn bench_create_vm(bencher: &mut Bencher) {
    let elf = load_elf("noop").unwrap();
    let loader_id = bpf_loader::id();
    with_mock_invoke_context(loader_id, 10000001, |invoke_context| {
        const BUDGET: u64 = 200_000;
        let compute_meter = invoke_context.get_compute_meter();
        {
            let mut compute_meter = compute_meter.borrow_mut();
            let to_consume = compute_meter.get_remaining() - BUDGET;
            compute_meter.consume(to_consume).unwrap();
        }

        // Serialize account data
        let keyed_accounts = invoke_context.get_keyed_accounts().unwrap();
        let (mut serialized, account_lengths) = serialize_parameters(
            &keyed_accounts[0].unsigned_key(),
            &keyed_accounts[1].unsigned_key(),
            &keyed_accounts[2..],
            &[],
        )
        .unwrap();

        let executable = <dyn Executable<BpfError, ThisInstructionMeter>>::from_elf(
            &elf,
            None,
            Config::default(),
            register_syscalls(invoke_context).unwrap(),
        )
        .unwrap();

        bencher.iter(|| {
            let _ = create_vm(
                &loader_id,
                executable.as_ref(),
                serialized.as_slice_mut(),
                invoke_context,
                &account_lengths,
            )
            .unwrap();
        });
    });
}

#[bench]
fn bench_instruction_count_tuner(_bencher: &mut Bencher) {
    let elf = load_elf("tuner").unwrap();
    let loader_id = bpf_loader::id();
    with_mock_invoke_context(loader_id, 10000001, |invoke_context| {
        const BUDGET: u64 = 200_000;
        let compute_meter = invoke_context.get_compute_meter();
        {
            let mut compute_meter = compute_meter.borrow_mut();
            let to_consume = compute_meter.get_remaining() - BUDGET;
            compute_meter.consume(to_consume).unwrap();
        }

        // Serialize account data
        let keyed_accounts = invoke_context.get_keyed_accounts().unwrap();
        let (mut serialized, account_lengths) = serialize_parameters(
            &keyed_accounts[0].unsigned_key(),
            &keyed_accounts[1].unsigned_key(),
            &keyed_accounts[2..],
            &[],
        )
        .unwrap();

        let executable = <dyn Executable<BpfError, ThisInstructionMeter>>::from_elf(
            &elf,
            None,
            Config::default(),
            register_syscalls(invoke_context).unwrap(),
        )
        .unwrap();
        let mut instruction_meter = ThisInstructionMeter { compute_meter };
        let mut vm = create_vm(
            &loader_id,
            executable.as_ref(),
            serialized.as_slice_mut(),
            invoke_context,
            &account_lengths,
        )
        .unwrap();

        let mut measure = Measure::start("tune");
        let _ = vm.execute_program_interpreted(&mut instruction_meter);
        measure.stop();

        assert_eq!(
            0,
            instruction_meter.get_remaining(),
            "Tuner must consume the whole budget"
        );
        println!(
            "{:?} compute units took {:?} us ({:?} instructions)",
            BUDGET - instruction_meter.get_remaining(),
            measure.as_us(),
            vm.get_total_instruction_count(),
        );
    });
}
