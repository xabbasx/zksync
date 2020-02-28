const ethers = require("ethers")
const { expect, use } = require("chai")
const { createMockProvider, getWallets, solidity, deployContract } = require("ethereum-waffle");
const { bigNumberify, parseEther, hexlify, formatEther } = require("ethers/utils");
const abi = require('ethereumjs-abi')

const SKIP_TEST = false;

// For: geth

// const provider = new ethers.providers.JsonRpcProvider(process.env.WEB3_URL);
// const wallet = ethers.Wallet.fromMnemonic(process.env.MNEMONIC, "m/44'/60'/0'/0/1").connect(provider);
// const exitWallet = ethers.Wallet.fromMnemonic(process.env.MNEMONIC, "m/44'/60'/0'/0/2").connect(provider);

// For: ganache

const provider = createMockProvider() //{gasLimit: 7000000, gasPrice: 2000000000});
const [wallet, wallet1, wallet2, exitWallet]  = getWallets(provider);

use(solidity);

async function deployTestContract(file) {
    try {
        return await deployContract(wallet, require(file), [], {
            gasLimit: 6000000,
        })
    } catch (err) {
        console.log('Error deploying', file, ': ', err)
    }
}

async function deployProxyContract(
    wallet,
    proxyCode,
    contractCode,
    initArgs,
    initArgsValues,
) {
    try {
        const proxy = await deployContract(wallet, proxyCode, [], {
            gasLimit: 3000000,
        });
        const contract = await deployContract(wallet, contractCode, [], {
            gasLimit: 3000000,
        });
        const initArgsInBytes = await abi.rawEncode(initArgs, initArgsValues);
        const tx = await proxy.initialize(contract.address, initArgsInBytes);
        await tx.wait();

        const returnContract = new ethers.Contract(proxy.address, contractCode.interface, wallet);
        return [returnContract, contract.address];
    } catch (err) {
        console.log('Error deploying proxy contract: ', err)
    }
}

async function getCallRevertReason(f) {
    let revertReason = "VM did not revert"
    try {
        let r = await f()
    } catch(e) {
        revertReason = (e.reason && e.reason[0]) || e.results[e.hashes[0]].reason
    } 
    return revertReason
}

module.exports = {
    provider,
    wallet,
    wallet1,
    wallet2,
    exitWallet,
    deployTestContract,
    deployProxyContract,
    getCallRevertReason,
    SKIP_TEST
}
