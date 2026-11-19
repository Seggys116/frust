#include "llvm/IR/Module.h"
#include "llvm/IR/PassManager.h"
#include "llvm/Passes/PassBuilder.h"
#include "llvm/Plugins/PassPlugin.h"

namespace {

struct SortDeclarations : llvm::PassInfoMixin<SortDeclarations> {
  llvm::PreservedAnalyses run(llvm::Module &module,
                             llvm::ModuleAnalysisManager &) {
    auto before = [](const auto &left, const auto &right) {
      if (left.isDeclaration() != right.isDeclaration())
        return !left.isDeclaration();
      return left.isDeclaration() && left.getName() < right.getName();
    };
    // Sort declarations without reordering definitions.
    module.getFunctionList().sort(before);
    return llvm::PreservedAnalyses::none();
  }
};

}

extern "C" llvm::PassPluginLibraryInfo llvmGetPassPluginInfo() {
  return {LLVM_PLUGIN_API_VERSION, "frust-sort-declarations", LLVM_VERSION_STRING,
          [](llvm::PassBuilder &builder) {
            builder.registerPipelineParsingCallback(
                [](llvm::StringRef name, llvm::ModulePassManager &manager,
                   llvm::ArrayRef<llvm::PassBuilder::PipelineElement>) {
                  if (name != "frust-sort-declarations")
                    return false;
                  manager.addPass(SortDeclarations());
                  return true;
                });
          }};
}
